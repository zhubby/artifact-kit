use artifactory_planner_runtime::lifecycle::Lifecycle;

#[test]
fn duplicate_process_cannot_claim_or_spend_another_owners_budget() {
    let mut run = Lifecycle::new(2);
    run.acquire(1, "pod-a", 1000, 600_000).unwrap();
    assert!(run.acquire(1, "pod-b", 1001, 600_000).is_err());
    assert!(run.reserve(1, "pod-b", 1001).is_err());
    assert_eq!(run.calls, 0);
    run.reserve(1, "pod-a", 1001).unwrap();
    assert_eq!(run.calls, 1);
}

#[test]
fn retry_fences_old_uid_without_resetting_budget_or_deadline() {
    let mut run = Lifecycle::new(2);
    run.acquire(1, "old", 1000, 600_000).unwrap();
    run.reserve(1, "old", 1001).unwrap();
    assert!(run.retry(2000).is_err());
    run.retry(92_000).unwrap();
    assert!(run.reserve(1, "old", 92_001).is_err());
    run.acquire(2, "new", 92_001, 600_000).unwrap();
    assert_eq!(run.deadline_ms, Some(601_000));
    assert_eq!(run.calls, 1);
    run.reserve(2, "new", 92_002).unwrap();
    assert!(run.reserve(2, "new", 92_003).is_err());
    assert!(run.retry(601_000).is_err());
}

#[test]
fn cancellation_and_pending_proposals_prevent_new_model_calls() {
    let mut run = Lifecycle::new(20);
    run.acquire(1, "pod", 1000, 600_000).unwrap();
    run.pending = true;
    assert!(run.reserve(1, "pod", 1001).is_err());
    assert!(run.retry(92_000).is_err());
    run.pending = false;
    run.canceled = true;
    assert!(run.valid(1, "pod", 1001).is_err());
    assert!(run.acquire(1, "pod", 1001, 600_000).is_err());
}
