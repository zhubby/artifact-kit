//! Shared source projections, valid in both embedded and standalone schemas.
use uuid::Uuid;

#[derive(Debug, toasty::Model)]
#[table = "credentials"]
pub(super) struct ServiceCredential {
    #[key]
    pub id: Uuid,
    pub revision: String,
    pub ciphertext: String,
}

#[derive(Debug, toasty::Model)]
#[table = "credentials"]
pub(super) struct ControlCredential {
    #[key]
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    pub ciphertext: String,
}

#[derive(Debug, toasty::Model)]
#[table = "source_repositories"]
pub(super) struct Repository {
    #[key]
    pub id: Uuid,
    pub project_id: Uuid,
    pub repository_url: String,
    pub credential_id: Option<Uuid>,
    #[unique]
    pub cache_key: String,
}

#[derive(Debug, toasty::Model)]
#[table = "source_snapshots"]
pub(super) struct Snapshot {
    #[key]
    pub id: Uuid,
    pub project_id: Uuid,
    pub root_repository_id: Uuid,
    pub commit_sha: String,
    #[column(type = jsonb)]
    pub manifest: toasty::Json<serde_json::Value>,
    pub digest: String,
    pub durable: bool,
}

#[derive(Debug, toasty::Model)]
#[table = "source_operations"]
pub(super) struct Operation {
    #[key]
    pub id: Uuid,
    pub input_hash: String,
    pub root_commit: Option<String>,
    pub snapshot_id: Option<Uuid>,
}

#[derive(Debug, toasty::Model)]
#[table = "source_operation_repos"]
#[key(operation_id, path)]
pub(super) struct OperationRepository {
    pub operation_id: Uuid,
    pub path: String,
    #[column(type = jsonb)]
    pub tags: toasty::Json<serde_json::Value>,
}

#[derive(Debug, toasty::Model)]
#[table = "source_retentions"]
#[key(snapshot_id, owner)]
pub(super) struct Retention {
    pub snapshot_id: Uuid,
    pub owner: String,
}
