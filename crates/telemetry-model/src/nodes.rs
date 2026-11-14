//! Bounded startup identity, resource freshness and ordered placement contracts.
use anyhow::{Result, ensure};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use uuid::Uuid;

/// Maximum encoded startup snapshot, including capabilities and startup identity.
pub const MAX_METADATA_BYTES: usize = 64 * 1024;
/// Maximum encoded node tool response, including pagination and timestamps.
pub const MAX_QUERY_BYTES: usize = 128 * 1024;

/// An authorized node and the current runtime identity whose resources may be used.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceIdentity {
    pub node_id: Uuid,
    pub startup_id: Uuid,
}

/// One immutable snapshot per runtime process; reconnects reuse exactly these values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataSnapshot {
    pub startup_id: Uuid,
    pub metadata: BTreeMap<String, Value>,
    pub capabilities: Vec<String>,
}
impl MetadataSnapshot {
    /// Construct a snapshot after validating reserved namespaces and the encoded budget.
    ///
    /// # Errors
    /// Rejects invalid keys, startup IDs, capabilities or oversized snapshots.
    pub fn new(
        startup_id: Uuid,
        metadata: BTreeMap<String, Value>,
        capabilities: Vec<String>,
    ) -> Result<Self> {
        let snapshot = Self {
            startup_id,
            metadata,
            capabilities,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }
    /// Validate data received over the authenticated node channel.
    ///
    /// # Errors
    /// Rejects unrecognized namespaces or values exceeding the snapshot budget.
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.startup_id.is_nil(), "invalid startup ID");
        ensure!(
            self.capabilities.len() <= 256
                && self
                    .capabilities
                    .iter()
                    .all(|v| !v.is_empty() && v.len() <= 128 && !v.contains('\0')),
            "invalid capabilities"
        );
        ensure!(self.metadata.len() <= 1024, "too many metadata keys");
        for key in self.metadata.keys() {
            ensure!(
                key.len() <= 256
                    && key.split('.').all(|part| !part.is_empty()
                        && part
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)))
                    && ["system.", "hardware.", "tools.", "emissary.", "custom."]
                        .iter()
                        .any(|prefix| key.starts_with(prefix)),
                "invalid metadata namespace"
            );
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_METADATA_BYTES,
            "metadata exceeds 64 KiB"
        );
        Ok(())
    }
    /// Hash canonical metadata and capabilities; process identity is checked separately.
    ///
    /// # Errors
    /// Returns JSON serialization errors.
    pub fn digest(&self) -> Result<String> {
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(&self.metadata, &self.capabilities))?)
        ))
    }
}

/// Resource thresholds checked against a fresh snapshot, without exclusive reservations.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRequirements {
    pub min_available_memory_bytes: Option<u64>,
    pub min_build_disk_available_bytes: Option<u64>,
    pub max_cpu_percent: Option<f64>,
}
impl ResourceRequirements {
    /// Validate representable capacities and normalized CPU utilization.
    ///
    /// # Errors
    /// Rejects out of range capacities and nonfinite percentages.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            [
                self.min_available_memory_bytes,
                self.min_build_disk_available_bytes
            ]
            .into_iter()
            .all(|v| v.is_none_or(|v| v <= i64::MAX as u64)),
            "invalid resource capacity"
        );
        ensure!(
            self.max_cpu_percent
                .is_none_or(|v| v.is_finite() && (0.0..=100.0).contains(&v)),
            "invalid CPU threshold"
        );
        Ok(())
    }
}
/// Latest host resources carry both event and server clocks and the runtime identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub startup_id: Uuid,
    pub sampled_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub available_memory_bytes: Option<u64>,
    pub build_disk_available_bytes: Option<u64>,
    pub cpu_percent: Option<f64>,
}
impl Resources {
    /// Accept current-startup samples only while both clocks satisfy the freshness window.
    #[must_use]
    pub fn fresh(&self, startup: Uuid, now: DateTime<Utc>) -> bool {
        self.startup_id == startup
            && [self.sampled_at, self.received_at]
                .into_iter()
                .all(|t| t >= now - Duration::seconds(90) && t <= now + Duration::seconds(30))
    }
    /// Unknown required metrics fail admission; optional unknown metrics remain unknown.
    #[must_use]
    pub fn satisfies(
        &self,
        startup: Uuid,
        required: &ResourceRequirements,
        now: DateTime<Utc>,
    ) -> bool {
        self.fresh(startup, now)
            && required
                .min_available_memory_bytes
                .is_none_or(|min| self.available_memory_bytes.is_some_and(|v| v >= min))
            && required
                .min_build_disk_available_bytes
                .is_none_or(|min| self.build_disk_available_bytes.is_some_and(|v| v >= min))
            && required.max_cpu_percent.is_none_or(|max| {
                self.cpu_percent
                    .is_some_and(|v| v.is_finite() && (0.0..=max).contains(&v))
            })
    }
}
/// A model-ranked device, fenced to the exact metadata used for the decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub node_id: Uuid,
    pub metadata_digest: String,
    pub reason: String,
}
/// Ordered candidates and target-wide resource requirements supplied by the Agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    pub candidates: Vec<Candidate>,
    #[serde(default)]
    pub resources: ResourceRequirements,
}
impl Placement {
    /// Validate bounds and candidate identity before persisting an execution plan.
    ///
    /// # Errors
    /// Rejects duplicates, missing reasons, invalid digests and oversized candidate lists.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.candidates.is_empty() && self.candidates.len() <= 8,
            "placement needs 1 to 8 candidates"
        );
        let mut ids = HashSet::new();
        for candidate in &self.candidates {
            ensure!(
                !candidate.node_id.is_nil() && ids.insert(candidate.node_id),
                "duplicate or invalid candidate"
            );
            ensure!(
                candidate.metadata_digest.len() == 64
                    && candidate
                        .metadata_digest
                        .bytes()
                        .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v)),
                "invalid metadata digest"
            );
            ensure!(
                !candidate.reason.trim().is_empty() && candidate.reason.len() <= 2000,
                "invalid placement reason"
            );
        }
        self.resources.validate()
    }
}
