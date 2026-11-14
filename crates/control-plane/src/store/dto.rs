//! Explicit API projections. These types never contain ORM models or database handles.
use artifactory_database::{Error, Result, sql::Decode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

macro_rules! response {
    ($name:ident { $($field:ident: $ty:ty),* $(,)? }) => {
        #[derive(Debug, Serialize, Deserialize)]
        pub struct $name { $(pub $field: $ty,)* }
        impl Decode for $name {
            fn value_type() -> toasty::stmt::Type { toasty::stmt::Type::String }
            fn decode(value: &toasty::stmt::Value) -> Result<Self> {
                serde_json::from_value(Value::decode(value)?)
                    .map_err(|_| Error::Decode(concat!("invalid ", stringify!($name), " projection")))
            }
        }
    };
}

response! { Overview {
    projects: i64,
    nodes_online: i64,
    nodes_registered: i64,
    running: i64,
    queued: i64,
    releases: i64,
    builds_today: i64,
}}

response! { UserSummary {
    id: Uuid,
    username: String,
    role: String,
    created_at: String,
}}

response! { TokenSummary {
    id: Uuid,
    user_id: Uuid,
    name: String,
    project_ids: Vec<Uuid>,
    scopes: Vec<String>,
    expires_at: String,
    revoked: bool,
    created_at: String,
}}

response! { PoolSummary {
    id: Uuid,
    name: String,
    emissary_target_release: Option<Uuid>,
    emissary_update_concurrency: i32,
    emissary_updates_paused: bool,
}}

response! { CredentialSummary {
    id: Uuid,
    name: String,
    kind: String,
    created_at: String,
}}

response! { AuditEvent {
    id: i64,
    actor: String,
    action: String,
    resource_id: String,
    details: Value,
    created_at: String,
}}

response! { NodeSummary {
    id: Uuid,
    name: String,
    os: String,
    arch: String,
    pool_id: Uuid,
    labels: Value,
    capabilities: Value,
    diagnostics: Value,
    signing_profiles: Vec<String>,
    slots: i32,
    draining: bool,
    revoked: bool,
    certificate_expires_at: String,
    last_seen: Option<String>,
    created_at: String,
    emissary_target_release: Option<Uuid>,
    emissary_current_version: Option<String>,
    emissary_launcher_version: Option<String>,
    emissary_ota_status: Value,
    ota_draining: bool,
    emissary_desired_release: Option<Uuid>,
    emissary_pool_paused: bool,
    pool_name: String,
    online: Option<bool>,
    active_jobs: i64,
}}

response! { Project {
    id: Uuid,
    name: String,
    repository_url: String,
    credential_id: Option<Uuid>,
    pool_ids: Vec<Uuid>,
    signing_profiles: Vec<String>,
    created_at: String,
    model_profile_id: Option<Uuid>,
    build_context: String,
    source_credentials: Value,
    planner_backend: String,
}}

response! { LastBuild {
    id: Uuid,
    status: String,
    created_at: String,
}}

response! { ProjectSummary {
    id: Uuid,
    name: String,
    repository_url: String,
    credential_id: Option<Uuid>,
    pool_ids: Vec<Uuid>,
    signing_profiles: Vec<String>,
    created_at: String,
    model_profile_id: Option<Uuid>,
    build_context: String,
    source_credentials: Value,
    planner_backend: String,
    build_count: i64,
    last_build: Option<LastBuild>,
}}

response! { ModelProfile {
    id: Uuid,
    name: String,
    api: String,
    base_url: String,
    model: String,
    parameters: Value,
    revision: i32,
    enabled: bool,
    is_default: bool,
    max_calls: i32,
    timeout_seconds: i32,
    created_at: String,
    next_call_at: Option<String>,
    has_key: bool,
    api_key: String,
}}

response! { ModelOption {
    id: Uuid,
    name: String,
    enabled: bool,
}}

response! { WorkerSummary {
    id: Uuid,
    name: String,
    certificate_expires_at: String,
    draining: bool,
    revoked: bool,
    last_seen: Option<String>,
    created_at: String,
    online: Option<bool>,
    active_run: Option<Uuid>,
}}

response! { SourceRepositorySummary {
    id: Uuid,
    project_id: Uuid,
    repository_url: String,
    credential_id: Option<Uuid>,
    last_used_at: String,
    fetched_bytes: i64,
    cache_hits: i64,
    cache_bytes: i64,
    git_download_bytes: i64,
    lfs_download_bytes: i64,
    emissary_cache_hits: i64,
    emissary_cache_misses: i64,
}}

response! { EmissaryRelease {
    id: Uuid,
    version: String,
    manifest: String,
    signature: String,
    manifest_hash: String,
    created_at: String,
}}

response! { EmissaryUpdate {
    id: Uuid,
    generation: i64,
    node_id: Uuid,
    pool_id: Uuid,
    release_id: Uuid,
    previous_version: Option<String>,
    status: String,
    cancel_requested: bool,
    error: Option<String>,
    retry_of: Option<Uuid>,
    created_at: String,
    updated_at: String,
    finished_at: Option<String>,
    last_command_at: Option<String>,
    version: String,
    node_name: String,
}}

response! { JobSummary {
    id: Uuid,
    target: String,
    status: String,
}}

response! { BuildSummary {
    id: Uuid,
    project_id: Uuid,
    requested_ref: String,
    commit_sha: Option<String>,
    parameters: Value,
    selected_targets: Vec<String>,
    status: String,
    error: Option<String>,
    release_version: Option<String>,
    publish_status: String,
    publish_error: Option<String>,
    cancel_requested: bool,
    requested_by: Option<Uuid>,
    created_at: String,
    finished_at: Option<String>,
    source_repository_url: String,
    source_credential_id: Option<Uuid>,
    goal: Option<String>,
    target_requirements: Value,
    planning_context: Value,
    source_snapshot_id: Option<Uuid>,
    revision_count: i32,
    clarification: Option<Value>,
    current_plan_id: Option<Uuid>,
    planner_backend: String,
    project_name: String,
    targets: Vec<JobSummary>,
}}

response! { BuildEnvelope {
    id: Uuid,
    project_id: Uuid,
    requested_ref: String,
    commit_sha: Option<String>,
    config_yaml: Option<String>,
    config_snapshot: Option<Value>,
    parameters: Value,
    selected_targets: Vec<String>,
    status: String,
    error: Option<String>,
    release_version: Option<String>,
    publish_status: String,
    publish_error: Option<String>,
    cancel_requested: bool,
    requested_by: Option<Uuid>,
    created_at: String,
    finished_at: Option<String>,
    source_repository_url: String,
    source_credential_id: Option<Uuid>,
    goal: Option<String>,
    target_requirements: Value,
    planning_context: Value,
    source_snapshot_id: Option<Uuid>,
    revision_count: i32,
    clarification: Option<Value>,
    current_plan_id: Option<Uuid>,
    planner_backend: String,
    project_name: String,
}}

response! { StepRun {
    attempt_id: Uuid,
    step_index: i32,
    name: String,
    phase: String,
    status: String,
    exit_code: Option<i32>,
    started_at: String,
    finished_at: Option<String>,
}}

response! { Attempt {
    id: Uuid,
    job_id: Uuid,
    node_id: Uuid,
    lease_expires_at: String,
    status: String,
    last_sequence: i64,
    error: Option<String>,
    started_at: Option<String>,
    finished_at: Option<String>,
    created_at: String,
    plan_revision_id: Option<Uuid>,
    source_snapshot_id: Option<Uuid>,
    failure_kind: String,
    node_name: String,
    steps: Vec<StepRun>,
}}

response! { Job {
    id: Uuid,
    build_id: Uuid,
    target: String,
    target_config: Value,
    status: String,
    created_at: String,
    plan_revision_id: Option<Uuid>,
    attempts: Vec<Attempt>,
    placement: Option<Value>,
}}

response! { BuildArtifact {
    id: Uuid,
    attempt_id: Uuid,
    path: String,
    size: i64,
    sha256: String,
    created_at: String,
    target: String,
    attempt_status: String,
}}

response! { PublishAttempt {
    id: Uuid,
    build_id: Uuid,
    status: String,
    error: Option<String>,
    created_at: String,
}}

#[derive(Debug, Serialize)]
pub struct BuildDetail {
    #[serde(flatten)]
    pub build: BuildEnvelope,
    pub jobs: Vec<Job>,
    pub artifacts: Vec<BuildArtifact>,
    pub publish_attempts: Vec<PublishAttempt>,
}

response! { ArtifactSummary {
    id: Uuid,
    attempt_id: Uuid,
    path: String,
    size: i64,
    sha256: String,
    created_at: String,
    target: String,
    build_id: Uuid,
    project_name: String,
    project_id: Uuid,
}}

response! { ReleaseSummary {
    id: Uuid,
    project_id: Uuid,
    build_id: Uuid,
    version: String,
    idempotency_key: String,
    commit_sha: String,
    created_at: String,
    project_name: String,
    artifact_count: i64,
}}

response! { Release {
    id: Uuid,
    project_id: Uuid,
    build_id: Uuid,
    version: String,
    idempotency_key: String,
    commit_sha: String,
    created_at: String,
    project_name: String,
}}

response! { ReleaseArtifact {
    id: Uuid,
    attempt_id: Uuid,
    path: String,
    size: i64,
    sha256: String,
    created_at: String,
    target: String,
}}

response! { PlanRevision {
    id: Uuid,
    build_id: Uuid,
    run_id: Uuid,
    revision: i32,
    source_snapshot_id: Uuid,
    contents: Value,
    sha256: String,
    created_at: String,
}}

response! { PlanningRun {
    id: Uuid,
    build_id: Uuid,
    worker_id: Option<Uuid>,
    purpose: String,
    state: String,
    failed_targets: Vec<String>,
    lease_expires_at: Option<String>,
    deadline_at: Option<String>,
    calls: i32,
    usage: Value,
    error: Option<String>,
    created_at: String,
    finished_at: Option<String>,
    authority: String,
}}

response! { PlanningEvent {
    id: i64,
    run_id: Uuid,
    kind: String,
    payload: Value,
    created_at: String,
}}

#[derive(Debug, Serialize)]
pub struct BuildPlans {
    pub plans: Vec<PlanRevision>,
    pub runs: Vec<PlanningRun>,
    pub events: Vec<PlanningEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn response_projections_preserve_wire_times_and_cannot_expose_secret_columns() {
        let id = Uuid::new_v4();
        let at = "2026-11-13T12:34:56.123456+00:00";
        let credential: CredentialSummary = serde_json::from_value(json!({
            "id":id,"name":"credential","kind":"ssh","created_at":at,
            "ciphertext":"must-never-leave-store"
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(credential).unwrap(),
            json!({
                "id":id,"name":"credential","kind":"ssh","created_at":at
            })
        );
        let user: UserSummary = serde_json::from_value(json!({
            "id":id,"username":"viewer","role":"viewer","created_at":at,
            "password_hash":"must-never-leave-store"
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(user).unwrap(),
            json!({
                "id":id,"username":"viewer","role":"viewer","created_at":at
            })
        );
    }
}
