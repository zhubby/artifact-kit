use anyhow::{Context, Result};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use uuid::Uuid;

#[derive(Clone, Default)]
pub struct Registry(Arc<Mutex<BTreeMap<Uuid, Root>>>);
#[derive(Clone)]
pub(super) struct Root {
    pub pid: u32,
    pub started_at: u64,
    pub step: Option<u32>,
}
pub struct Guard {
    registry: Registry,
    attempt: Uuid,
}
impl Drop for Guard {
    fn drop(&mut self) {
        if let Ok(mut roots) = self.registry.0.lock() {
            roots.remove(&self.attempt);
        }
    }
}
impl Registry {
    /// Capture start time before dispatching work, so PID reuse cannot adopt an unrelated process.
    pub async fn register(&self, attempt: Uuid, pid: u32) -> Result<Guard> {
        let started_at = tokio::task::spawn_blocking(move || {
            let mut s = System::new();
            let pid = Pid::from_u32(pid);
            s.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                true,
                ProcessRefreshKind::nothing().without_tasks(),
            );
            s.process(pid)
                .map(|p| p.start_time())
                .filter(|n| *n > 0)
                .context("managed worker start time unavailable")
        })
        .await??;
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("managed process registry poisoned"))?
            .insert(
                attempt,
                Root {
                    pid,
                    started_at,
                    step: None,
                },
            );
        Ok(Guard {
            registry: self.clone(),
            attempt,
        })
    }
    pub fn step(&self, attempt: Uuid, step: Option<u32>) {
        if let Ok(mut roots) = self.0.lock()
            && let Some(root) = roots.get_mut(&attempt)
        {
            root.step = step;
        }
    }
    pub(super) fn roots(&self) -> BTreeMap<Uuid, Root> {
        self.0.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

/// Include known descendants while their owner is registered, even if an intermediate shell exits.
pub(super) fn owners(
    system: &System,
    roots: &BTreeMap<Uuid, Root>,
    previous: &HashMap<(u32, u64), Uuid>,
) -> HashMap<(u32, u64), Uuid> {
    let mut result = HashMap::new();
    for (&id, root) in roots {
        if system
            .process(Pid::from_u32(root.pid))
            .is_some_and(|p| p.start_time() == root.started_at)
        {
            result.insert((root.pid, root.started_at), id);
        }
    }
    for (&identity, &owner) in previous {
        if roots.contains_key(&owner)
            && system
                .process(Pid::from_u32(identity.0))
                .is_some_and(|p| p.start_time() == identity.1)
        {
            result.insert(identity, owner);
        }
    }
    loop {
        let old = result.len();
        for (pid, p) in system.processes() {
            if let Some(parent) = p.parent().and_then(|pid| system.process(pid)) {
                let parent_pid = p.parent().map(|pid| pid.as_u32()).unwrap_or(0);
                if let Some(owner) = result.get(&(parent_pid, parent.start_time())).copied()
                    && p.start_time() >= parent.start_time()
                {
                    result.insert((pid.as_u32(), p.start_time()), owner);
                }
            }
        }
        if old == result.len() {
            return result;
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn only_managed_descendants_are_attributed_and_pid_reuse_is_rejected() {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 30 & wait"])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let id = Uuid::new_v4();
        let registry = Registry::default();
        let guard = registry.register(id, child.id().unwrap()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let s = System::new_all();
        let roots = registry.roots();
        let got = owners(&s, &roots, &HashMap::new());
        assert!(!got.is_empty());
        assert!(!got.keys().any(|(pid, _)| *pid == std::process::id()));
        let mut reused = roots.clone();
        reused.get_mut(&id).unwrap().started_at += 100;
        assert!(owners(&s, &reused, &HashMap::new()).is_empty());
        drop(guard);
        assert!(owners(&s, &registry.roots(), &got).is_empty());
        child.kill().await.unwrap();
    }
}
