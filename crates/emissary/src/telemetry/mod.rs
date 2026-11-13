//! Background telemetry with a bounded durable spool, independent of leases and build logs.
mod collector;
mod inventory;
mod managed;
mod spool;

use crate::{
    client,
    config::Config,
    doctor,
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

#[derive(Clone)]
pub struct Snapshot {
    pub diagnostics: Value,
    pub capabilities: Vec<String>,
    pub metrics: Value,
}
pub struct Handle {
    pub registry: Registry,
    snapshot: Arc<RwLock<Snapshot>>,
    cancel: CancellationToken,
}
impl Handle {
    /// Start best-effort background collection. Telemetry errors never propagate into execution.
    pub fn start(cfg: Config, diagnostics: Value, capabilities: Vec<String>) -> Self {
        let registry = Registry::default();
        let cancel = CancellationToken::new();
        let snapshot = Arc::new(RwLock::new(Snapshot {
            diagnostics: diagnostics.clone(),
            capabilities: capabilities.clone(),
            metrics: json!({}),
        }));
        let spool = Arc::new(Mutex::new(None::<Spool>));
        let notify = Arc::new(Notify::new());
        let (probe_tx, mut probe_rx) = tokio::sync::watch::channel((diagnostics, capabilities));
        let probe_cfg = cfg.clone();
        let probe_cancel = cancel.clone();
        let probe_snapshot = snapshot.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {_=probe_cancel.cancelled()=>break,_=tokio::time::sleep(Duration::from_secs(900))=>{}}
                let value = tokio::select! {_=probe_cancel.cancelled()=>break,value=doctor::doctor(&probe_cfg)=>value};
                if let Ok(mut snapshot) = probe_snapshot.write() {
                    snapshot.diagnostics = value.0.clone();
                    snapshot.capabilities = value.1.clone();
                }
                if probe_tx.send(value).is_err() {
                    break;
                }
            }
        });
        let collect_cfg = cfg.clone();
        let collect_cancel = cancel.clone();
        let collect_registry = registry.clone();
        let collect_spool = spool.clone();
        let collect_snapshot = snapshot.clone();
        let collect_notify = notify.clone();
        tokio::spawn(async move {
            let Ok(mut collector) = tokio::task::spawn_blocking(Collector::new).await else {
                return;
            };
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut index = 0u64;
            let mut cards = Vec::new();
            let mut fingerprint = Vec::new();
            loop {
                tokio::select! {_=collect_cancel.cancelled()=>break,_=tick.tick()=>{}}
                let host = index.is_multiple_of(3);
                let probe = (index.is_multiple_of(90) || probe_rx.has_changed().unwrap_or(false))
                    .then(|| probe_rx.borrow_and_update().clone());
                let cfg = collect_cfg.clone();
                let registry = collect_registry.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let data = collector.sample(&cfg, &registry, host, probe);
                    (collector, data)
                })
                .await;
                let Ok((next, (mut batch, metrics))) = result else {
                    break;
                };
                collector = next;
                if let Some(inventory) = &mut batch.inventory {
                    cards = tokio::select! {_=collect_cancel.cancelled()=>break,v=inventory::accelerators(&collect_cfg)=>v};
                    inventory.accelerators = cards.clone();
                    inventory.collectors.insert(
                        "accelerator_inventory".into(),
                        if cards.is_empty() {
                            "unknown_or_unsupported"
                        } else {
                            "available"
                        }
                        .into(),
                    );
                    if cards.iter().any(|c| c.vendor == "NVIDIA") {
                        inventory
                            .collectors
                            .insert("accelerator_utilization".into(), "nvidia-smi".into());
                    }
                    let next = serde_json::to_vec(inventory).unwrap_or_default();
                    if next == fingerprint && !index.is_multiple_of(360) {
                        batch.inventory = None;
                    } else {
                        fingerprint = next;
                    }
                }
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
                let _ = tokio::task::spawn_blocking(move || {
                    let Ok(mut guard) = spool.lock() else { return };
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
                                return;
                            }
                        }
                    }
                    if let Some(spool) = guard.as_mut() {
                        if batch.validate().is_err() || spool.push(&mut batch).is_err() {
                            let _ = spool.note_gap();
                            eprintln!(
                                "telemetry sample could not be buffered; execution continues"
                            );
                        }
                        if let Ok(mut snapshot) = snapshot.write() {
                            snapshot.metrics["telemetry_dropped_batches"] = json!(spool.dropped());
                        }
                    }
                })
                .await;
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
                    tokio::select! {_=upload_cancel.cancelled()=>break,_=tokio::time::sleep(Duration::from_secs(backoff))=>{}}
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
