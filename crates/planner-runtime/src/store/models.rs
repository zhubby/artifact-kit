//! Runtime records are private to the persistence layer.
use uuid::Uuid;

#[derive(Debug, toasty::Model)]
#[table = "runtime_runs"]
pub(super) struct Run {
    #[key]
    pub id: Uuid,
    pub version: i64,
    pub intent_hash: Option<String>,
    pub intent_ciphertext: Option<String>,
    pub project_id: Option<Uuid>,
    pub pool: Option<String>,
    pub snapshot_id: Option<Uuid>,
    pub snapshot_digest: Option<String>,
    #[column(type = jsonb)]
    pub lifecycle: toasty::Json<serde_json::Value>,
    pub status: String,
    #[column(type = jsonb)]
    pub execution: Option<toasty::Json<serde_json::Value>>,
    #[column(type = jsonb)]
    pub checkpoint: Option<toasty::Json<serde_json::Value>>,
    #[column(type = jsonb)]
    #[default(toasty::Json(serde_json::json!({})))]
    pub usage: toasty::Json<serde_json::Value>,
    #[default(0)]
    pub report_sequence: i64,
    pub grant_hash: Option<String>,
    pub grant_ciphertext: Option<String>,
}

#[derive(Debug, toasty::Model)]
#[table = "runtime_profiles"]
pub(super) struct Profile {
    #[key]
    pub id: Uuid,
}

#[derive(Debug, toasty::Model)]
#[table = "runtime_model_requests"]
#[key(run_id, request_id)]
pub(super) struct ModelRequest {
    pub run_id: Uuid,
    pub request_id: Uuid,
    pub input_hash: String,
    pub generation: i64,
    #[default("reserved".to_owned())]
    pub state: String,
}

#[derive(Debug, toasty::Model)]
#[table = "runtime_outbox"]
pub(super) struct Outbox {
    #[key]
    pub id: Uuid,
    pub run_id: Uuid,
    pub kind: String,
    #[column(type = jsonb)]
    pub payload: toasty::Json<serde_json::Value>,
    #[default(false)]
    pub delivered: bool,
}

#[derive(Debug, toasty::Model)]
#[table = "runtime_reports"]
#[key(run_id, event_id)]
pub(super) struct Report {
    pub run_id: Uuid,
    pub event_id: Uuid,
    pub input_hash: String,
}

#[derive(Debug, toasty::Model)]
#[table = "runtime_proposals"]
pub(super) struct Proposal {
    #[key]
    pub id: Uuid,
    pub run_id: Uuid,
    pub generation: i64,
    pub input_hash: String,
    pub kind: String,
    #[column(type = jsonb)]
    pub payload: toasty::Json<serde_json::Value>,
    pub deadline_ms: i64,
    #[column(type = jsonb)]
    pub result: Option<toasty::Json<serde_json::Value>>,
}
