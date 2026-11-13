use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lifecycle {
    pub generation: i64,
    pub owner: Option<String>,
    pub calls: u32,
    pub max_calls: u32,
    pub deadline_ms: Option<i64>,
    pub lease_until_ms: Option<i64>,
    pub canceled: bool,
    pub pending: bool,
}
impl Lifecycle {
    pub fn new(max_calls: u32) -> Self {
        Self {
            generation: 1,
            owner: None,
            calls: 0,
            max_calls,
            deadline_ms: None,
            lease_until_ms: None,
            canceled: false,
            pending: false,
        }
    }
    pub fn acquire(&mut self, generation: i64, uid: &str, now: i64, timeout: i64) -> Result<()> {
        ensure!(
            !self.canceled && !self.pending,
            "run canceled or awaiting acceptance"
        );
        ensure!(
            generation == self.generation && !uid.is_empty(),
            "execution fenced"
        );
        ensure!(
            self.owner.as_deref().is_none_or(|owner| owner == uid),
            "execution already claimed"
        );
        ensure!(
            self.deadline_ms.is_none_or(|deadline| deadline > now),
            "planning deadline reached"
        );
        // Reacquiring the same identity cannot resurrect an expired lease.
        ensure!(
            self.owner.is_none() || self.lease_until_ms.is_some_and(|until| until > now),
            "lease expired"
        );
        let deadline = *self.deadline_ms.get_or_insert(now + timeout);
        self.owner = Some(uid.into());
        self.lease_until_ms = Some((now + 90_000).min(deadline));
        Ok(())
    }
    pub fn valid(&self, generation: i64, uid: &str, now: i64) -> Result<()> {
        ensure!(!self.canceled, "run canceled");
        ensure!(
            generation == self.generation && self.owner.as_deref() == Some(uid),
            "execution fenced"
        );
        ensure!(
            self.deadline_ms.is_some_and(|until| until > now),
            "planning deadline reached"
        );
        ensure!(
            self.lease_until_ms.is_some_and(|until| until > now),
            "lease expired"
        );
        Ok(())
    }
    pub fn reserve(&mut self, generation: i64, uid: &str, now: i64) -> Result<()> {
        self.valid(generation, uid, now)?;
        ensure!(!self.pending, "proposal awaiting acceptance");
        ensure!(self.calls < self.max_calls, "model call budget exhausted");
        self.calls += 1;
        Ok(())
    }
    pub fn retry(&mut self, now: i64) -> Result<()> {
        ensure!(
            !self.canceled && !self.pending,
            "run canceled or awaiting acceptance"
        );
        ensure!(
            self.lease_until_ms.is_none_or(|until| until <= now),
            "lease still live"
        );
        ensure!(
            self.deadline_ms.is_none_or(|until| until > now),
            "planning deadline reached"
        );
        self.generation += 1;
        self.owner = None;
        self.lease_until_ms = None;
        Ok(())
    }
}
