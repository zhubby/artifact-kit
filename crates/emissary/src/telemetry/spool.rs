use crate::config::{atomic_json, private_dir, read_json};
use anyhow::{Context, Result, ensure};
use artifactory_telemetry_model::{Batch, MAX_BATCH_BYTES};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, fs, path::PathBuf};

const MAX_RECORDS: usize = 8192;
#[derive(Default, Serialize, Deserialize)]
struct Stats {
    dropped: u64,
}
pub(super) struct Spool {
    root: PathBuf,
    quota: u64,
    bytes: u64,
    next: u64,
    records: VecDeque<(PathBuf, u64)>,
    stats: Stats,
    _lock: fs::File,
}
impl Spool {
    pub fn open(root: PathBuf, quota: u64) -> Result<Self> {
        private_dir(&root)?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.join("lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock).context("telemetry spool already in use")?;
        let stats = match read_json(&root.join("stats.json")) {
            Ok(s) => s,
            Err(_) if !root.join("stats.json").exists() => Stats::default(),
            Err(e) => return Err(e),
        };
        let mut records = Vec::new();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".batch") && entry.file_type()?.is_file() {
                records.push((entry.path(), entry.metadata()?.len()));
            }
        }
        records.sort_by(|a, b| a.0.cmp(&b.0));
        let next = records
            .last()
            .and_then(|r| r.0.file_name())
            .and_then(|n| n.to_str())
            .and_then(|s| s.split('-').next())
            .and_then(|s| s.parse::<u64>().ok())
            .map_or(0, |n| n.saturating_add(1));
        let mut spool = Self {
            root,
            quota,
            bytes: records.iter().map(|r| r.1).sum(),
            next,
            records: records.into(),
            stats,
            _lock: lock,
        };
        while spool.bytes > quota || spool.records.len() > MAX_RECORDS {
            spool.discard()?;
        }
        Ok(spool)
    }
    pub fn push(&mut self, batch: &mut Batch) -> Result<()> {
        // Re-encode after each eviction so the durable record exposes the resulting gap counter.
        loop {
            batch.dropped_batches = self.stats.dropped;
            let data = serde_json::to_vec(batch)?;
            ensure!(
                data.len() <= MAX_BATCH_BYTES && data.len() as u64 <= self.quota,
                "telemetry batch exceeds quota"
            );
            if self.bytes + data.len() as u64 > self.quota || self.records.len() >= MAX_RECORDS {
                self.discard()?;
                continue;
            }
            let path = self
                .root
                .join(format!("{:020}-{}.batch", self.next, batch.id));
            self.next = self
                .next
                .checked_add(1)
                .context("telemetry spool sequence exhausted")?;
            crate::config::atomic_bytes(&path, &data, false)?;
            self.bytes += data.len() as u64;
            self.records.push_back((path, data.len() as u64));
            return Ok(());
        }
    }
    pub fn first(&self) -> Result<Option<(PathBuf, Vec<u8>)>> {
        let Some((path, length)) = self.records.front() else {
            return Ok(None);
        };
        ensure!(
            *length <= MAX_BATCH_BYTES as u64,
            "invalid telemetry spool entry"
        );
        Ok(Some((path.clone(), fs::read(path)?)))
    }
    pub fn ack(&mut self, path: &std::path::Path) -> Result<()> {
        // The collector may have evicted an in-flight record while the service was slow.
        if self.records.front().is_some_and(|r| r.0 == path) {
            self.remove_first()?;
        }
        Ok(())
    }
    pub fn discard(&mut self) -> Result<()> {
        self.note_gap()?;
        self.remove_first()
    }
    pub fn note_gap(&mut self) -> Result<()> {
        self.stats.dropped = self.stats.dropped.saturating_add(1).min(i64::MAX as u64);
        atomic_json(&self.root.join("stats.json"), &self.stats)?;
        Ok(())
    }
    fn remove_first(&mut self) -> Result<()> {
        if let Some((path, length)) = self.records.front() {
            fs::remove_file(path)?;
            #[cfg(unix)]
            fs::File::open(&self.root)?.sync_all()?;
            self.bytes = self.bytes.saturating_sub(*length);
            self.records.pop_front();
        }
        Ok(())
    }
    pub fn dropped(&self) -> u64 {
        self.stats.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;
    fn batch() -> Batch {
        Batch {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            sampled_at: Utc::now(),
            monotonic_ms: 1,
            dropped_batches: 0,
            omitted_processes: 0,
            host: None,
            processes: vec![],
            inventory: None,
        }
    }
    #[test]
    fn quota_eviction_survives_restart_and_late_ack() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Spool::open(dir.path().into(), 800).unwrap();
        s.push(&mut batch()).unwrap();
        let (first, _) = s.first().unwrap().unwrap();
        for _ in 0..8 {
            s.push(&mut batch()).unwrap();
        }
        assert!(s.bytes <= 800);
        assert!(s.dropped() > 0);
        s.ack(&first).unwrap();
        let dropped = s.dropped();
        let bytes = s.first().unwrap().unwrap().1;
        drop(s);
        let mut s = Spool::open(dir.path().into(), 800).unwrap();
        assert_eq!(s.dropped(), dropped);
        assert_eq!(s.first().unwrap().unwrap().1, bytes);
        let (path, _) = s.first().unwrap().unwrap();
        s.ack(&path).unwrap();
    }
}
