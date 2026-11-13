use super::*;
use crate::config::read_json;
use std::path::Path;

#[test]
fn bounded_spool_does_not_skip_sequence_on_overflow() {
    let root = tempfile::tempdir().unwrap();
    let mut spool = Spool::new(root.path().into(), 1).unwrap();
    let mut sequence = 0;
    assert!(
        spool
            .append("attempt", &mut sequence, "test", b"data")
            .is_err()
    );
    assert_eq!((sequence, spool.bytes), (0, 0));
    spool.limit = 4096;
    spool
        .append("attempt", &mut sequence, "test", b"data")
        .unwrap();
    assert!(
        root.path()
            .join("spool/attempt/00000000000000000001.json")
            .exists()
    );
}
#[test]
fn duplicate_assignment_and_delayed_renewal_do_not_extend_lease() {
    let root = tempfile::tempdir().unwrap();
    let mut state = test_state(root.path());
    let now = Instant::now();
    let id = uuid::Uuid::new_v4().to_string();
    let assign = pb::Command {
        kind: "assign".into(),
        attempt_id: id.clone(),
        lease_token: "lease".into(),
        lease_seconds: 90,
        assignment: Some(pb::Assignment::default()),
        ..Default::default()
    };
    state.command(assign.clone(), now).unwrap();
    let deadline = state.tasks[&id].lease.deadline();
    state.command(assign, now + Duration::from_secs(5)).unwrap();
    assert_eq!(state.tasks[&id].lease.deadline(), deadline);
    state.tasks.get_mut(&id).unwrap().started = true;
    state.anchors.insert("request".into(), now);
    state
        .command(
            pb::Command {
                kind: "renew".into(),
                attempt_id: id.clone(),
                lease_token: "lease".into(),
                request_id: "request".into(),
                lease_seconds: 30,
                ..Default::default()
            },
            now + Duration::from_secs(15),
        )
        .unwrap();
    assert_eq!(
        state.tasks[&id].lease.deadline(),
        now + Duration::from_secs(30)
    );
    state
        .command(
            pb::Command {
                kind: "renew".into(),
                attempt_id: id.clone(),
                lease_token: "lease".into(),
                request_id: "request".into(),
                lease_seconds: 90,
                ..Default::default()
            },
            now + Duration::from_secs(31),
        )
        .unwrap();
    assert!(!state.tasks.contains_key(&id));
}
#[test]
fn ota_drain_waits_for_terminal_ack_workers_and_logs() {
    let root = tempfile::tempdir().unwrap();
    let mut state = test_state(root.path());
    state.ota_directory = Some(root.path().into());
    atomic_json(
        &root.path().join("state.json"),
        &ota::State {
            update_id: "u".into(),
            phase: "draining".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    state.tasks.insert(
        id.clone(),
        Task::new(
            "lease".into(),
            pb::Assignment::default(),
            90,
            Instant::now(),
        ),
    );
    state.workers = 1;
    state.spool.bytes = 5;
    assert!(!state.ota_snapshot("ready").unwrap().activation_pending);
    state.tasks.remove(&id);
    assert!(!state.ota_snapshot("ready").unwrap().activation_pending);
    state.workers = 0;
    assert!(!state.ota_snapshot("ready").unwrap().activation_pending);
    state.spool.bytes = 0;
    assert!(state.ota_snapshot("ready").unwrap().activation_pending);
    assert!(!root.path().join("drained.json").exists());
    state.anchors.insert("ready".into(), Instant::now());
    state
        .command(
            pb::Command {
                kind: "emissary_health".into(),
                request_id: "ready".into(),
                ..Default::default()
            },
            Instant::now(),
        )
        .unwrap();
    assert!(root.path().join("drained.json").exists());
}
#[test]
fn ota_health_requires_current_outstanding_heartbeat() {
    let root = tempfile::tempdir().unwrap();
    let mut state = test_state(root.path());
    state.ota_directory = Some(root.path().into());
    let now = Instant::now();
    state.anchors.insert("fresh".into(), now);
    state
        .anchors
        .insert("expired".into(), now - Duration::from_secs(60));
    for id in ["forged", "expired"] {
        state
            .command(
                pb::Command {
                    kind: "emissary_health".into(),
                    request_id: id.into(),
                    ..Default::default()
                },
                now,
            )
            .unwrap();
    }
    assert!(!root.path().join("health.json").exists());
    state
        .command(
            pb::Command {
                kind: "emissary_health".into(),
                request_id: "fresh".into(),
                ..Default::default()
            },
            now,
        )
        .unwrap();
    assert_eq!(
        read_json::<ota::Health>(&root.path().join("health.json"))
            .unwrap()
            .id,
        "fresh"
    );
    assert_eq!(state.health_at, Some(now));
}
fn test_state(root: &Path) -> State {
    State::new(
        Config {
            root: root.into(),
            log_buffer_bytes: 4096,
            ..Default::default()
        },
        json!({}),
        vec![],
    )
    .unwrap()
}
