//! Bounded telemetry records. Environment values and process command lines are intentionally absent.
pub mod nodes;
use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use uuid::Uuid;

/// Maximum encoded batch, independent of the build log quota.
pub const MAX_BATCH_BYTES: usize = 1_048_576;
/// Maximum managed processes in one sampling interval; omissions are explicitly counted.
pub const MAX_PROCESSES: usize = 2048;

/// A replayable, immutable collection from one Emissary sampling interval.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    pub id: Uuid,
    pub session_id: Uuid,
    pub sampled_at: DateTime<Utc>,
    pub monotonic_ms: u64,
    pub dropped_batches: u64,
    pub omitted_processes: u32,
    pub host: Option<HostSample>,
    pub processes: Vec<ProcessSample>,
    pub inventory: Option<Inventory>,
}

/// Host percentages are normalized to 0–100; missing metrics are unknown, never synthesized zero.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSample {
    pub cpu_percent: Option<f64>,
    pub memory_total_bytes: Option<u64>,
    pub memory_used_bytes: Option<u64>,
    #[serde(default)]
    pub available_memory_bytes: Option<u64>,
    #[serde(default)]
    pub build_disk_available_bytes: Option<u64>,
    pub swap_total_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    pub load_one: Option<f64>,
    pub disks: Vec<DiskSample>,
    pub networks: Vec<NetworkSample>,
    pub accelerators: Vec<AcceleratorSample>,
    pub active_attempts: u32,
}

/// Filesystem capacity and optional cumulative physical I/O counters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskSample {
    pub device: String,
    pub mount: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub read_bytes: Option<u64>,
    pub written_bytes: Option<u64>,
}

/// Interface counters are cumulative within an OS/interface lifetime, not rates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkSample {
    pub interface: String,
    pub received_bytes: u64,
    pub transmitted_bytes: u64,
    pub receive_errors: u64,
    pub transmit_errors: u64,
}

/// Accelerator statistics supplied only by an available vendor collector.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceleratorSample {
    pub id: String,
    pub utilization_percent: Option<f64>,
    pub memory_used_bytes: Option<u64>,
}

/// Only a managed worker or its descendants may appear here. CPU can exceed 100 on many cores.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessSample {
    pub attempt_id: Uuid,
    pub pid: u32,
    pub started_at: u64,
    pub parent_pid: Option<u32>,
    pub step: Option<u32>,
    pub cpu_percent: Option<f64>,
    pub rss_bytes: Option<u64>,
    pub read_bytes: Option<u64>,
    pub written_bytes: Option<u64>,
    pub status: String,
}

/// Change-versioned machine and toolchain inventory; version probes do not certify native builds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub emissary_version: String,
    pub os: String,
    pub os_version: Option<String>,
    pub kernel_version: Option<String>,
    pub arch: String,
    pub boot_time: Option<u64>,
    pub cpu_model: Option<String>,
    pub logical_cpus: Option<u32>,
    pub physical_cores: Option<u32>,
    pub physical_memory_bytes: Option<u64>,
    pub effective_cpu_limit: Option<f64>,
    pub effective_memory_limit_bytes: Option<u64>,
    pub container: Option<String>,
    pub virtualization: Option<String>,
    pub accelerators: Vec<Accelerator>,
    pub tools: BTreeMap<String, Tool>,
    pub capabilities: Vec<String>,
    pub signing_profile_refs: Vec<String>,
    pub collectors: BTreeMap<String, String>,
}

/// Hardware identity with driver information, without speculative performance scores.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Accelerator {
    pub id: String,
    pub vendor: String,
    pub model: String,
    pub driver: Option<String>,
    pub memory_total_bytes: Option<u64>,
}

/// A bounded version probe, not a build verification or signing credential.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tool {
    pub available: bool,
    pub version: Option<String>,
}

impl Batch {
    /// Validate bounds and values before ingestion; encoded byte size is checked by the transport.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.monotonic_ms <= i64::MAX as u64,
            "invalid monotonic clock"
        );
        ensure!(
            self.dropped_batches <= i64::MAX as u64,
            "invalid dropped counter"
        );
        ensure!(self.processes.len() <= MAX_PROCESSES, "too many processes");
        let finite = |v: Option<f64>, max| v.is_none_or(|v| v.is_finite() && v >= 0.0 && v <= max);
        let bytes = |v: Option<u64>| v.is_none_or(|v| v <= i64::MAX as u64);
        let text = |v: &str, max| v.len() <= max && !v.contains('\0');
        let mut identities = HashSet::new();
        for p in &self.processes {
            ensure!(
                p.pid > 0 && p.started_at > 0 && p.started_at <= i64::MAX as u64,
                "invalid process identity"
            );
            ensure!(
                identities.insert((p.pid, p.started_at)),
                "duplicate process identity"
            );
            ensure!(
                finite(p.cpu_percent, 1_000_000.0)
                    && bytes(p.rss_bytes)
                    && bytes(p.read_bytes)
                    && bytes(p.written_bytes),
                "invalid process metric"
            );
            ensure!(text(&p.status, 64), "invalid process status");
        }
        if let Some(h) = &self.host {
            ensure!(
                h.active_attempts <= i32::MAX as u32,
                "invalid active attempt count"
            );
            ensure!(
                finite(h.cpu_percent, 100.0) && finite(h.load_one, 1_000_000.0),
                "invalid host metric"
            );
            ensure!(
                [
                    h.memory_total_bytes,
                    h.memory_used_bytes,
                    h.available_memory_bytes,
                    h.build_disk_available_bytes,
                    h.swap_total_bytes,
                    h.swap_used_bytes
                ]
                .into_iter()
                .all(bytes),
                "invalid memory metric"
            );
            ensure!(
                h.disks.len() <= 256 && h.networks.len() <= 256 && h.accelerators.len() <= 64,
                "too many host devices"
            );
            for d in &h.disks {
                ensure!(
                    text(&d.device, 512)
                        && text(&d.mount, 1024)
                        && bytes(Some(d.total_bytes))
                        && d.available_bytes <= d.total_bytes
                        && bytes(d.read_bytes)
                        && bytes(d.written_bytes),
                    "invalid disk metric"
                );
            }
            for n in &h.networks {
                ensure!(
                    text(&n.interface, 128)
                        && [
                            n.received_bytes,
                            n.transmitted_bytes,
                            n.receive_errors,
                            n.transmit_errors
                        ]
                        .into_iter()
                        .all(|n| bytes(Some(n))),
                    "invalid interface metric"
                );
            }
            for g in &h.accelerators {
                ensure!(
                    text(&g.id, 256)
                        && finite(g.utilization_percent, 100.0)
                        && bytes(g.memory_used_bytes),
                    "invalid accelerator metric"
                );
            }
        }
        if let Some(i) = &self.inventory {
            ensure!(
                i.tools.len() <= 256
                    && i.capabilities.len() <= 256
                    && i.signing_profile_refs.len() <= 32
                    && i.accelerators.len() <= 64
                    && i.collectors.len() <= 32,
                "oversized inventory"
            );
            ensure!(
                [&i.emissary_version, &i.os, &i.arch]
                    .into_iter()
                    .all(|v| text(v, 128)),
                "invalid inventory identity"
            );
            ensure!(
                [
                    &i.os_version,
                    &i.kernel_version,
                    &i.cpu_model,
                    &i.container,
                    &i.virtualization
                ]
                .into_iter()
                .all(|v| v.as_deref().is_none_or(|v| text(v, 512))),
                "invalid inventory description"
            );
            ensure!(
                finite(i.effective_cpu_limit, 1_000_000.0)
                    && bytes(i.physical_memory_bytes)
                    && bytes(i.effective_memory_limit_bytes)
                    && bytes(i.boot_time),
                "invalid inventory capacity"
            );
            ensure!(i.tools.iter().all(|(k,t)|text(k,128) && t.version.as_deref().is_none_or(|v|text(v,512))), "invalid tool probe");
            ensure!(
                i.capabilities
                    .iter()
                    .chain(&i.signing_profile_refs)
                    .all(|v| text(v, 128)),
                "invalid capability"
            );
            ensure!(
                i.collectors
                    .iter()
                    .all(|(k, v)| text(k, 128) && text(v, 128)),
                "invalid collector status"
            );
            for g in &i.accelerators {
                ensure!(
                    [&g.id, &g.vendor, &g.model]
                        .into_iter()
                        .all(|v| text(v, 256))
                        && g.driver.as_deref().is_none_or(|v| text(v, 256))
                        && bytes(g.memory_total_bytes),
                    "invalid accelerator inventory"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_process_duplicates_and_secret_fields() {
        let mut b = Batch {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            sampled_at: Utc::now(),
            monotonic_ms: 0,
            dropped_batches: 0,
            omitted_processes: 0,
            host: None,
            processes: vec![],
            inventory: None,
        };
        let p = ProcessSample {
            attempt_id: Uuid::new_v4(),
            pid: 1,
            started_at: 100,
            parent_pid: None,
            step: None,
            cpu_percent: None,
            rss_bytes: None,
            read_bytes: None,
            written_bytes: None,
            status: "running".into(),
        };
        b.processes = vec![p.clone(), p];
        assert!(b.validate().is_err());
        b.processes.pop();
        assert!(b.validate().is_ok());
        let mut value = serde_json::to_value(b).unwrap();
        value["processes"][0]["command_line"] = "secret".into();
        assert!(serde_json::from_value::<Batch>(value).is_err());
    }
}
