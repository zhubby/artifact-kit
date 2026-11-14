use super::managed::{self, Registry};
use crate::config::Config;
use artifactory_telemetry_model::{
    Batch, DiskSample, HostSample, Inventory, MAX_PROCESSES, NetworkSample, ProcessSample,
};
use chrono::Utc;
use serde_json::{Value, json};
use std::{collections::HashMap, time::Instant};
use sysinfo::{Disks, Networks, Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use uuid::Uuid;

pub(super) struct Collector {
    system: System,
    disks: Disks,
    networks: Networks,
    session: Uuid,
    started: Instant,
    previous: HashMap<(u32, u64), Uuid>,
    cpu_ready: bool,
}
impl Collector {
    pub fn new(session: Uuid) -> Self {
        let mut system = System::new();
        system.refresh_cpu_all();
        system.refresh_memory();
        Self {
            system,
            disks: Disks::new_with_refreshed_list(),
            networks: Networks::new_with_refreshed_list(),
            session,
            started: Instant::now(),
            previous: HashMap::new(),
            cpu_ready: false,
        }
    }
    pub fn sample(
        &mut self,
        cfg: &Config,
        registry: &Registry,
        host: bool,
        inventory: Option<Inventory>,
    ) -> (Batch, Value) {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        let roots = registry.roots();
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().without_tasks(),
        );
        let owners = managed::owners(&self.system, &roots, &self.previous);
        let mut pids: Vec<_> = owners.keys().map(|(pid, _)| Pid::from_u32(*pid)).collect();
        pids.push(Pid::from_u32(std::process::id()));
        // Enumerate ancestry locally, then read resource counters only for managed processes.
        // Never request command lines, environment, cwd, executable paths or user IDs.
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            false,
            ProcessRefreshKind::nothing()
                .without_tasks()
                .with_cpu()
                .with_memory()
                .with_disk_usage(),
        );
        let mut identities: Vec<_> = owners.iter().collect();
        identities.sort_by_key(|(identity, _)| **identity);
        let omitted = identities
            .len()
            .saturating_sub(MAX_PROCESSES)
            .min(u32::MAX as usize) as u32;
        let processes = identities
            .into_iter()
            .take(MAX_PROCESSES)
            .filter_map(|(&(pid, started), &attempt)| {
                let p = self.system.process(Pid::from_u32(pid))?;
                let disk = p.disk_usage();
                Some(ProcessSample {
                    attempt_id: attempt,
                    pid,
                    started_at: started,
                    parent_pid: p.parent().map(|p| p.as_u32()),
                    step: roots.get(&attempt).and_then(|r| r.step),
                    cpu_percent: self
                        .previous
                        .contains_key(&(pid, started))
                        .then_some(f64::from(p.cpu_usage())),
                    rss_bytes: Some(p.memory()),
                    read_bytes: if cfg!(any(target_os = "linux", target_os = "macos", windows)) {
                        Some(disk.total_read_bytes)
                    } else {
                        None
                    },
                    written_bytes: if cfg!(any(target_os = "linux", target_os = "macos", windows)) {
                        Some(disk.total_written_bytes)
                    } else {
                        None
                    },
                    status: p.status().to_string(),
                })
            })
            .collect();
        self.previous = owners;
        let h = host.then(|| {
            self.disks.refresh(true);
            self.networks.refresh(true);
            HostSample {
                cpu_percent: self
                    .cpu_ready
                    .then_some(f64::from(self.system.global_cpu_usage())),
                memory_total_bytes: Some(self.system.total_memory()).filter(|n| *n > 0),
                memory_used_bytes: Some(self.system.used_memory())
                    .filter(|_| self.system.total_memory() > 0),
                available_memory_bytes: super::inventory::available_memory(&self.system),
                build_disk_available_bytes: fs2::available_space(&cfg.root).ok(),
                swap_total_bytes: Some(self.system.total_swap()),
                swap_used_bytes: Some(self.system.used_swap()),
                load_one: if cfg!(unix) {
                    Some(System::load_average().one)
                } else {
                    None
                },
                disks: self
                    .disks
                    .list()
                    .iter()
                    .filter(|d| d.total_space() > 0)
                    .take(256)
                    .map(|d| {
                        let usage = d.usage();
                        DiskSample {
                            device: d.name().to_string_lossy().chars().take(512).collect(),
                            mount: d
                                .mount_point()
                                .to_string_lossy()
                                .chars()
                                .take(1024)
                                .collect(),
                            total_bytes: d.total_space(),
                            available_bytes: d.available_space(),
                            read_bytes: if cfg!(any(target_os = "linux", windows)) {
                                Some(usage.total_read_bytes)
                            } else {
                                None
                            },
                            written_bytes: if cfg!(any(target_os = "linux", windows)) {
                                Some(usage.total_written_bytes)
                            } else {
                                None
                            },
                        }
                    })
                    .collect(),
                networks: self
                    .networks
                    .iter()
                    .take(256)
                    .map(|(name, n)| NetworkSample {
                        interface: name.chars().take(128).collect(),
                        received_bytes: n.total_received(),
                        transmitted_bytes: n.total_transmitted(),
                        receive_errors: n.total_errors_on_received(),
                        transmit_errors: n.total_errors_on_transmitted(),
                    })
                    .collect(),
                active_attempts: roots.len() as u32,
                ..HostSample::default()
            }
        });
        self.cpu_ready = true;
        let metrics = json!({"logical_cpus":self.system.cpus().len(),"emissary_rss_bytes":self.system.process(Pid::from_u32(std::process::id())).map(|p|p.memory()),"managed_free_bytes":fs2::available_space(&cfg.root).ok()});
        (
            Batch {
                id: Uuid::new_v4(),
                session_id: self.session,
                sampled_at: Utc::now(),
                monotonic_ms: self.started.elapsed().as_millis().min(i64::MAX as u128) as u64,
                dropped_batches: 0,
                omitted_processes: omitted,
                host: h,
                processes,
                inventory,
            },
            metrics,
        )
    }
}
