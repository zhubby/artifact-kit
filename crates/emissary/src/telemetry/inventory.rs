use crate::{config::Config, process};
use artifactory_telemetry_model::{Accelerator, AcceleratorSample, Inventory, Tool};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};
use sysinfo::System;
use tokio::process::Command;

pub(super) fn collect(
    system: &System,
    diagnostics: &Value,
    capabilities: Vec<String>,
) -> Inventory {
    let inventory = Inventory {
        emissary_version: crate::VERSION.into(),
        os: crate::platform().0.into(),
        arch: crate::platform().1.into(),
        os_version: System::long_os_version(),
        kernel_version: System::kernel_version(),
        boot_time: Some(System::boot_time()).filter(|n| *n > 0),
        cpu_model: system
            .cpus()
            .first()
            .map(|c| c.brand().to_owned())
            .filter(|s| !s.is_empty()),
        logical_cpus: (!system.cpus().is_empty()).then_some(system.cpus().len() as u32),
        physical_cores: System::physical_core_count().map(|n| n as u32),
        physical_memory_bytes: Some(system.total_memory()).filter(|n| *n > 0),
        effective_cpu_limit: std::thread::available_parallelism()
            .ok()
            .map(|n| n.get() as f64),
        tools: diagnostics["tools"]
            .as_object()
            .map(|tools| {
                tools
                    .iter()
                    .map(|(name, v)| {
                        (
                            name.clone(),
                            Tool {
                                available: v["available"].as_bool().unwrap_or(false),
                                version: v["version"]
                                    .as_str()
                                    .filter(|s| !s.is_empty())
                                    .map(str::to_owned),
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
        signing_profile_refs: diagnostics["configured_signing_profiles"]
            .as_array()
            .map(|v| {
                v.iter()
                    .filter_map(|s| s.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        capabilities,
        collectors: BTreeMap::from([
            ("host".into(), "available".into()),
            ("managed_processes".into(), "available".into()),
            ("accelerator_utilization".into(), "unsupported".into()),
            ("virtualization".into(), "unknown".into()),
        ]),
        ..Inventory::default()
    };
    #[cfg(target_os = "linux")]
    let inventory = {
        let mut inventory = inventory;
        linux_limits(&mut inventory);
        inventory
    };
    inventory
}

#[cfg(target_os = "linux")]
fn linux_limits(inventory: &mut Inventory) {
    use std::{
        fs,
        path::{Component, Path, PathBuf},
    };
    inventory.container = if Path::new("/.dockerenv").exists() {
        Some("docker".into())
    } else if Path::new("/run/.containerenv").exists() {
        Some("oci".into())
    } else if fs::read_to_string("/proc/1/cgroup").is_ok_and(|s| s.contains("kubepods")) {
        Some("kubernetes".into())
    } else {
        None
    };
    let root = Path::new("/sys/fs/cgroup");
    let relative = fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("0::").map(str::to_owned))
        })
        .unwrap_or_default();
    let mut dir = PathBuf::from(root);
    for component in Path::new(&relative).components() {
        if let Component::Normal(s) = component {
            dir.push(s);
        }
    }
    if !dir.exists() {
        dir = root.into();
    }
    // Walk ancestors: a leaf may say "max" while a parent enforces a tighter quota.
    loop {
        if let Ok(value) = fs::read_to_string(dir.join("cpu.max")) {
            let mut parts = value.split_whitespace();
            if let (Some(quota), Some(period)) = (
                parts.next().and_then(|s| s.parse::<f64>().ok()),
                parts.next().and_then(|s| s.parse::<f64>().ok()),
            ) && period > 0.0
            {
                inventory.effective_cpu_limit = Some(
                    inventory
                        .effective_cpu_limit
                        .unwrap_or(f64::MAX)
                        .min(quota / period),
                );
            }
        }
        if let Ok(value) = fs::read_to_string(dir.join("memory.max"))
            && let Ok(limit) = value.trim().parse::<u64>()
        {
            inventory.effective_memory_limit_bytes = Some(
                inventory
                    .effective_memory_limit_bytes
                    .unwrap_or(u64::MAX)
                    .min(limit),
            );
        }
        if dir == root || !dir.pop() {
            break;
        }
    }
    // cgroup v1 hosts expose controller-specific files instead of unified cpu.max.
    if let (Ok(quota), Ok(period)) = (
        fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us"),
        fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us"),
    ) && let (Ok(q), Ok(p)) = (quota.trim().parse::<f64>(), period.trim().parse::<f64>())
        && q > 0.0
        && p > 0.0
    {
        inventory.effective_cpu_limit =
            Some(inventory.effective_cpu_limit.unwrap_or(f64::MAX).min(q / p));
    }
    if inventory.effective_memory_limit_bytes.is_none()
        && let Ok(value) = fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes")
        && let Ok(limit) = value.trim().parse::<u64>()
        && limit < 1 << 60
    {
        inventory.effective_memory_limit_bytes = Some(limit);
    }
    let dmi = ["sys_vendor", "product_name"]
        .into_iter()
        .filter_map(|name| fs::read_to_string(format!("/sys/class/dmi/id/{name}")).ok())
        .collect::<Vec<_>>()
        .join(" ");
    let lower = dmi.to_lowercase();
    if ["kvm", "qemu", "vmware", "virtual", "xen", "amazon ec2"]
        .iter()
        .any(|s| lower.contains(s))
    {
        inventory.virtualization = Some(dmi.trim().chars().take(256).collect());
        inventory
            .collectors
            .insert("virtualization".into(), "available".into());
    }
}

async fn output(tool: &str, args: &[&str]) -> Option<String> {
    let mut command = Command::new(tool);
    command.args(args);
    let (status, bytes) = process::probe(command, Duration::from_secs(4)).await.ok()?;
    status
        .success()
        .then(|| String::from_utf8_lossy(&bytes).into_owned())
}
pub(super) async fn accelerators(cfg: &Config) -> Vec<Accelerator> {
    if let Some(text) = output(
        &cfg.tool("nvidia-smi"),
        &[
            "--query-gpu=uuid,name,driver_version,memory.total",
            "--format=csv,noheader,nounits",
        ],
    )
    .await
    {
        return text
            .lines()
            .take(64)
            .filter_map(|line| {
                let p: Vec<_> = line.split(',').map(str::trim).collect();
                (p.len() == 4).then(|| Accelerator {
                    id: p[0].into(),
                    vendor: "NVIDIA".into(),
                    model: p[1].into(),
                    driver: Some(p[2].into()),
                    memory_total_bytes: p[3]
                        .parse::<u64>()
                        .ok()
                        .and_then(|n| n.checked_mul(1 << 20)),
                })
            })
            .collect();
    }
    #[cfg(target_os = "macos")]
    if let Some(text) = output(
        "/usr/sbin/system_profiler",
        &["SPDisplaysDataType", "-json"],
    )
    .await
        && let Ok(value) = serde_json::from_str::<Value>(&text)
        && let Some(displays) = value["SPDisplaysDataType"].as_array()
    {
        return displays
            .iter()
            .enumerate()
            .take(64)
            .map(|(index, v)| Accelerator {
                id: format!("display-{index}"),
                vendor: v["spdisplays_vendor"]
                    .as_str()
                    .unwrap_or("unknown")
                    .chars()
                    .take(256)
                    .collect(),
                model: v["sppci_model"]
                    .as_str()
                    .or_else(|| v["_name"].as_str())
                    .unwrap_or("unknown")
                    .chars()
                    .take(256)
                    .collect(),
                driver: None,
                // Unified memory is not dedicated VRAM; leave it unknown.
                memory_total_bytes: None,
            })
            .collect();
    }
    #[cfg(windows)]
    if let Some(text)=output("powershell",&["-NoProfile","-NonInteractive","-Command","@(Get-CimInstance Win32_VideoController | Select-Object PNPDeviceID,Name,DriverVersion,AdapterRAM) | ConvertTo-Json -Compress"]).await
        && let Ok(value)=serde_json::from_str::<Value>(&text) && let Some(cards)=value.as_array() {
        return cards.iter().take(64).map(|v|Accelerator{id:v["PNPDeviceID"].as_str().unwrap_or("unknown").chars().take(256).collect(),vendor:"unknown".into(),model:v["Name"].as_str().unwrap_or("unknown").chars().take(256).collect(),driver:v["DriverVersion"].as_str().map(str::to_owned),memory_total_bytes:v["AdapterRAM"].as_u64().filter(|n|*n>0)}).collect();
    }
    Vec::new()
}
pub(super) async fn accelerator_samples(
    cfg: &Config,
    cards: &[Accelerator],
) -> Vec<AcceleratorSample> {
    let mut samples: Vec<_> = cards
        .iter()
        .map(|g| AcceleratorSample {
            id: g.id.clone(),
            utilization_percent: None,
            memory_used_bytes: None,
        })
        .collect();
    if cards.iter().any(|c| c.vendor == "NVIDIA")
        && let Some(text) = output(
            &cfg.tool("nvidia-smi"),
            &[
                "--query-gpu=uuid,utilization.gpu,memory.used",
                "--format=csv,noheader,nounits",
            ],
        )
        .await
    {
        for line in text.lines() {
            let p: Vec<_> = line.split(',').map(str::trim).collect();
            if p.len() == 3
                && let Some(s) = samples.iter_mut().find(|s| s.id == p[0])
            {
                s.utilization_percent = p[1]
                    .parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite() && (0.0..=100.0).contains(n));
                s.memory_used_bytes = p[2]
                    .parse::<u64>()
                    .ok()
                    .and_then(|n| n.checked_mul(1 << 20));
            }
        }
    }
    samples
}
