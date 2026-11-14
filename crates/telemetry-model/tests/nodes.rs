use artifactory_telemetry_model::nodes::{
    MetadataSnapshot, Placement, ResourceRequirements, Resources,
};
use chrono::{Duration, Utc};
use serde_json::json;
use std::collections::BTreeMap;
use uuid::Uuid;

#[test]
fn metadata_is_canonical_bounded_and_namespaced() {
    let id = Uuid::new_v4();
    let snapshot = MetadataSnapshot::new(
        id,
        BTreeMap::from([("system.os".into(), json!("linux"))]),
        vec!["git".into()],
    )
    .unwrap();
    assert_eq!(
        snapshot.digest().unwrap(),
        snapshot.clone().digest().unwrap()
    );
    assert!(
        MetadataSnapshot::new(
            id,
            BTreeMap::from([("labels.admin".into(), json!(true))]),
            vec![]
        )
        .is_err()
    );
    assert!(
        MetadataSnapshot::new(
            id,
            BTreeMap::from([("custom.large".into(), json!("x".repeat(65536)))]),
            vec![]
        )
        .is_err()
    );
    assert!(
        MetadataSnapshot::new(
            id,
            BTreeMap::from([("custom.site".into(), json!("lab"))]),
            vec![]
        )
        .is_ok()
    );
}

#[test]
fn resource_admission_requires_current_startup_and_both_clocks() {
    let now = Utc::now();
    let startup = Uuid::new_v4();
    let mut resources = Resources {
        startup_id: startup,
        sampled_at: now,
        received_at: now,
        available_memory_bytes: Some(4096),
        build_disk_available_bytes: Some(8192),
        cpu_percent: Some(30.0),
    };
    let required = ResourceRequirements {
        min_available_memory_bytes: Some(1024),
        min_build_disk_available_bytes: Some(2048),
        max_cpu_percent: Some(50.0),
    };
    assert!(resources.satisfies(startup, &required, now));
    assert!(!resources.satisfies(Uuid::new_v4(), &required, now));
    resources.sampled_at = now - Duration::seconds(91);
    assert!(!resources.satisfies(startup, &required, now));
    resources.sampled_at = now;
    resources.received_at = now - Duration::seconds(91);
    assert!(!resources.satisfies(startup, &required, now));
    resources.received_at = now;
    resources.available_memory_bytes = None;
    assert!(!resources.satisfies(startup, &required, now));
}

#[test]
fn placement_rejects_duplicates_and_unbounded_candidates() {
    let candidate = json!({"node_id": Uuid::new_v4(), "metadata_digest":"a".repeat(64),"reason":"native toolchain"});
    let invalid: Placement =
        serde_json::from_value(json!({"candidates":[candidate.clone(),candidate],"resources":{}}))
            .unwrap();
    assert!(invalid.validate().is_err());
    let bad: ResourceRequirements = serde_json::from_value(json!({"max_cpu_percent":101})).unwrap();
    assert!(bad.validate().is_err());
}
