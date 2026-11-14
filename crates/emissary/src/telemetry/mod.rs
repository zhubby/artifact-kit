//! Background telemetry with a bounded durable spool, independent of leases and build logs.
mod collector;
mod inventory;
mod managed;
mod spool;

use crate::{
    client,
    config::Config,
    protocol::{TelemetryBatch, telemetry_client::TelemetryClient},
};
use collector::Collector;
pub use managed::Registry;
use serde_json::{Value, json};
use spool::Spool;
use std::{
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Capture static observations once, before the runtime opens any control connection.
pub async fn startup(
    cfg: &Config,
    startup_id: uuid::Uuid,
    diagnostics: &Value,
    capabilities: Vec<String>,
) -> anyhow::Result<(
    artifactory_telemetry_model::nodes::MetadataSnapshot,
    artifactory_telemetry_model::Inventory,
)> {
    let mut system = sysinfo::System::new();
    system.refresh_cpu_all();
    system.refresh_memory();
    let mut inventory = inventory::collect(&system, diagnostics, capabilities.clone());
    inventory.accelerators = inventory::accelerators(cfg).await;
    inventory.collectors.insert(
        "accelerator_inventory".into(),
        if inventory.accelerators.is_empty() {
            "unknown_or_unsupported"
        } else {
            "available"
        }
        .into(),
    );
    if inventory
        .accelerators
        .iter()
        .any(|card| card.vendor == "NVIDIA")
    {
        inventory
            .collectors
            .insert("accelerator_utilization".into(), "nvidia-smi".into());
    }
    let mut metadata = std::collections::BTreeMap::new();
    let value = serde_json::to_value(&inventory)?;
    for (key, value) in value
        .as_object()
        .expect("inventory serializes to an object")
    {
        let namespace = match key.as_str() {
            "tools" => "tools",
            "emissary_version" | "collectors" => "emissary",
            "os" | "os_version" | "kernel_version" | "arch" | "boot_time" | "container"
            | "virtualization" => "system",
            "capabilities" | "signing_profile_refs" => continue,
            _ => "hardware",
        };
        if key == "tools" {
            if let Some(tools) = value.as_object() {
                for (tool, value) in tools {
                    metadata.insert(format!("tools.{tool}"), value.clone());
                }
            }
        } else if !value.is_null() {
            metadata.insert(format!("{namespace}.{key}"), value.clone());
        }
    }
    for (key, value) in &cfg.metadata {
        metadata.insert(format!("custom.{key}"), value.clone());
    }
    let snapshot = artifactory_telemetry_model::nodes::MetadataSnapshot::new(
        startup_id,
        metadata,
        capabilities,
    )?;
    Ok((snapshot, inventory))
}

#[derive(Clone)]
pub struct Snapshot {
    pub metrics: Value,
}
pub struct Handle {
    pub registry: Registry,
    snapshot: Arc<RwLock<Snapshot>>,
    cancel: CancellationToken,
}
impl Handle {
    /// Start best-effort background collection. Telemetry errors never propagate into execution.
    pub fn start(
        cfg: Config,
        startup_id: uuid::Uuid,
        inventory: artifactory_telemetry_model::Inventory,
    ) -> Self {
        let registry = Registry::default();
        let cancel = CancellationToken::new();
        let snapshot = Arc::new(RwLock::new(Snapshot { metrics: json!({}) }));
        let spool = Arc::new(Mutex::new(None::<Spool>));
        let notify = Arc::new(Notify::new());
        let collect_cfg = cfg.clone();
        let collect_cancel = cancel.clone();
        let collect_registry = registry.clone();
        let collect_spool = spool.clone();
        let collect_snapshot = snapshot.clone();
        let collect_notify = notify.clone();
        tokio::spawn(async move {
            let Ok(mut collector) =
                tokio::task::spawn_blocking(move || Collector::new(startup_id)).await
            else {
                return;
            };
            let start = tokio::time::Instant::now() + crate::client::retry_delay(10);
            let mut tick = tokio::time::interval_at(start, Duration::from_secs(10));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut index = 0u64;
            let cards = inventory.accelerators.clone();
            let mut pending_inventory = Some(inventory);
            loop {
                tokio::select! {_=collect_cancel.cancelled()=>break,_=tick.tick()=>{}}
                let host = index.is_multiple_of(3);
                let inventory = pending_inventory.clone();
                let cfg = collect_cfg.clone();
                let registry = collect_registry.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let data = collector.sample(&cfg, &registry, host, inventory);
                    (collector, data)
                })
                .await;
                let Ok((next, (mut batch, metrics))) = result else {
                    break;
                };
                collector = next;
                if let Some(host) = &mut batch.host {
                    host.accelerators = tokio::select! {_=collect_cancel.cancelled()=>break,v=inventory::accelerator_samples(&collect_cfg,&cards)=>v};
                }
                if let Ok(mut snapshot) = collect_snapshot.write() {
                    snapshot.metrics = metrics;
                }
                index = index.saturating_add(1);
                if batch.host.is_none() && batch.processes.is_empty() && batch.inventory.is_none() {
                    continue;
                }
                let spool = collect_spool.clone();
                let cfg = collect_cfg.clone();
                let snapshot = collect_snapshot.clone();
                let buffered = tokio::task::spawn_blocking(move || {
                    let mut guard = spool.lock().expect("telemetry spool lock poisoned");
                    if guard.is_none() {
                        match Spool::open(
                            cfg.root.join("telemetry"),
                            if cfg.telemetry_buffer_bytes == 0 {
                                32 << 20
                            } else {
                                cfg.telemetry_buffer_bytes
                            },
                        ) {
                            Ok(s) => *guard = Some(s),
                            Err(_) => {
                                eprintln!("telemetry spool unavailable; execution continues");
                                return false;
                            }
                        }
                    }
                    let mut buffered = false;
                    if let Some(spool) = guard.as_mut() {
                        if batch.validate().is_err() || spool.push(&mut batch).is_err() {
                            let _ = spool.note_gap();
                            eprintln!(
                                "telemetry sample could not be buffered; execution continues"
                            );
                        } else {
                            buffered = true;
                        }
                        if let Ok(mut snapshot) = snapshot.write() {
                            snapshot.metrics["telemetry_dropped_batches"] = json!(spool.dropped());
                        }
                    }
                    buffered
                })
                .await
                .unwrap_or(false);
                if buffered {
                    pending_inventory = None;
                }
                collect_notify.notify_one();
            }
        });
        let upload_cancel = cancel.clone();
        tokio::spawn(async move {
            let mut endpoint = cfg.clone();
            endpoint.endpoint = cfg.telemetry_endpoint.clone();
            let mut connection = None;
            let mut backoff = 1u64;
            loop {
                if upload_cancel.is_cancelled() {
                    break;
                }
                let local = spool.clone();
                let result = tokio::task::spawn_blocking(
                    move || -> anyhow::Result<Option<(std::path::PathBuf, Vec<u8>)>> {
                        let mut guard = local
                            .lock()
                            .map_err(|_| anyhow::anyhow!("telemetry spool poisoned"))?;
                        let Some(spool) = guard.as_mut() else {
                            return Ok(None);
                        };
                        match spool.first() {
                            Ok(value) => Ok(value),
                            Err(_) => {
                                spool.discard()?;
                                Ok(None)
                            }
                        }
                    },
                )
                .await;
                let Ok(Ok(Some((path, bytes)))) = result else {
                    tokio::select! {_=upload_cancel.cancelled()=>break,_=notify.notified()=>{},_=tokio::time::sleep(Duration::from_secs(10))=>{}};
                    continue;
                };
                let expected = serde_json::from_slice::<artifactory_telemetry_model::Batch>(&bytes)
                    .ok()
                    .map(|b| b.id.to_string());
                let operation = async {
                    if connection.is_none() {
                        connection = Some(TelemetryClient::new(
                            client::dial(&endpoint, false).await.map_err(|_| {
                                tonic::Status::unavailable("telemetry connection unavailable")
                            })?,
                        ));
                    }
                    let client = connection.as_mut().ok_or_else(|| {
                        tonic::Status::unavailable("telemetry connection unavailable")
                    })?;
                    client
                        .upload(TelemetryBatch { json: bytes })
                        .await
                        .map(|r| r.into_inner())
                };
                let result = tokio::select! {_=upload_cancel.cancelled()=>break,r=tokio::time::timeout(Duration::from_secs(20),operation)=>r};
                let (remove, drop_record) = match result {
                    Ok(Ok(ack)) if Some(&ack.batch_id) == expected.as_ref() => {
                        backoff = 1;
                        (true, false)
                    }
                    Ok(Err(status))
                        if matches!(
                            status.code(),
                            tonic::Code::InvalidArgument
                                | tonic::Code::OutOfRange
                                | tonic::Code::AlreadyExists
                                | tonic::Code::PermissionDenied
                        ) =>
                    {
                        eprintln!(
                            "telemetry record rejected ({:?}); gap recorded",
                            status.code()
                        );
                        (true, true)
                    }
                    _ => {
                        connection = None;
                        (false, false)
                    }
                };
                if remove {
                    let local = spool.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Ok(mut guard) = local.lock()
                            && let Some(spool) = guard.as_mut()
                        {
                            if drop_record {
                                if spool.first().ok().flatten().is_some_and(|r| r.0 == path) {
                                    let _ = spool.discard();
                                }
                            } else {
                                let _ = spool.ack(&path);
                            }
                        }
                    })
                    .await;
                } else {
                    tokio::select! {_=upload_cancel.cancelled()=>break,_=tokio::time::sleep(crate::client::retry_delay(backoff))=>{}}
                    backoff = (backoff * 2).min(30);
                }
            }
        });
        Self {
            registry,
            snapshot,
            cancel,
        }
    }
    /// Read cached observations without any OS probes on the lease heartbeat path.
    pub fn snapshot(&self) -> Option<Snapshot> {
        self.snapshot.read().ok().map(|s| s.clone())
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
