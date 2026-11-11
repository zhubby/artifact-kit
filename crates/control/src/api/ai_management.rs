use crate::{
    auth::Auth,
    crypto,
    error::{ApiError, Result},
    state::AppState,
};
use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileInput {
    name: String,
    api: String,
    base_url: String,
    model: String,
    #[serde(default)]
    api_key: String,
    #[serde(default = "object")]
    parameters: Value,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    is_default: bool,
    #[serde(default = "calls")]
    max_calls: i32,
    #[serde(default = "timeout")]
    timeout_seconds: i32,
}
fn object() -> Value {
    json!({})
}
fn yes() -> bool {
    true
}
fn calls() -> i32 {
    20
}
fn timeout() -> i32 {
    600
}

fn validate(state: &AppState, input: &ProfileInput) -> Result<()> {
    let url = url::Url::parse(&input.base_url)
        .map_err(|_| ApiError::BadRequest("invalid model endpoint".into()))?;
    if input.name.trim().is_empty()
        || input.name.len() > 100
        || input.model.is_empty()
        || input.model.len() > 200
        || ![
            "openai-completions",
            "openai-responses",
            "anthropic-messages",
        ]
        .contains(&input.api.as_str())
        || url.host_str().is_none()
        || !(url.scheme() == "https" || state.settings.insecure_dev && url.scheme() == "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !input.parameters.is_object()
        || input.parameters.to_string().len() > 8192
        || input.api_key.len() > 8192
        || input.api_key.contains(['\0', '\r', '\n'])
        || !(1..=100).contains(&input.max_calls)
        || !(30..=3600).contains(&input.timeout_seconds)
    {
        return Err(ApiError::BadRequest("invalid model configuration".into()));
    }
    // Parameters are model inference options, never an alternate auth/endpoint channel.
    let allowed = [
        "thinking_level",
        "max_tokens",
        "context_window",
        "temperature",
        "requests_per_minute",
    ];
    if input
        .parameters
        .as_object()
        .unwrap()
        .keys()
        .any(|key| !allowed.contains(&key.as_str()))
    {
        return Err(ApiError::BadRequest("unsupported model parameter".into()));
    }
    for (key, value) in input.parameters.as_object().unwrap() {
        let valid = match key.as_str() {
            "thinking_level" => value.as_str().is_some_and(|level| {
                ["off", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&level)
            }),
            "max_tokens" => value.as_u64().is_some_and(|n| (1..=1_000_000).contains(&n)),
            "context_window" => value
                .as_u64()
                .is_some_and(|n| (1024..=10_000_000).contains(&n)),
            "requests_per_minute" => value.as_u64().is_some_and(|n| (1..=600).contains(&n)),
            "temperature" => value.as_f64().is_some_and(|n| (0.0..=2.0).contains(&n)),
            _ => false,
        };
        if !valid {
            return Err(ApiError::BadRequest(format!(
                "invalid model parameter: {key}"
            )));
        }
    }
    Ok(())
}

pub async fn profiles(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Value>>> {
    auth.session_role("admin")?;
    Ok(Json(sqlx::query_scalar("SELECT (to_jsonb(m)-'key_ciphertext') || jsonb_build_object('has_key',key_ciphertext<>'','api_key','********') FROM model_profiles m ORDER BY created_at")
        .fetch_all(&state.pool).await?))
}

pub async fn model_options(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Value>>> {
    auth.session_role("operator")?;
    Ok(Json(sqlx::query_scalar("SELECT jsonb_build_object('id',id,'name',name,'enabled',enabled) FROM model_profiles WHERE enabled ORDER BY name").fetch_all(&state.pool).await?))
}
pub async fn create_profile(
    State(state): State<AppState>,
    auth: Auth,
    Json(input): Json<ProfileInput>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    validate(&state, &input)?;
    if input.api_key.is_empty() {
        return Err(ApiError::BadRequest("model API key is required".into()));
    }
    let id = Uuid::new_v4();
    save(&state, id, input, false).await?;
    state
        .audit(auth.user_id, "model_profile_created", id, json!({}))
        .await?;
    Ok(Json(json!({"id":id})))
}
pub async fn update_profile(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(input): Json<ProfileInput>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    validate(&state, &input)?;
    save(&state, id, input, true).await?;
    state
        .audit(auth.user_id, "model_profile_updated", id, json!({}))
        .await?;
    Ok(Json(json!({"ok":true})))
}
async fn save(state: &AppState, id: Uuid, input: ProfileInput, update: bool) -> Result<()> {
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(724319911)")
        .execute(&mut *tx)
        .await?;
    if input.is_default {
        sqlx::query("UPDATE model_profiles SET is_default=false WHERE is_default")
            .execute(&mut *tx)
            .await?;
    }
    let ciphertext = if input.api_key.is_empty() || input.api_key == "********" {
        if !update {
            return Err(ApiError::BadRequest("model API key is required".into()));
        }
        sqlx::query_scalar::<_, String>("SELECT key_ciphertext FROM model_profiles WHERE id=$1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?
    } else {
        crypto::seal(&state.settings.master_key, input.api_key.as_bytes())?
    };
    let affected=if update {
        sqlx::query("UPDATE model_profiles SET name=$2,api=$3,base_url=$4,model=$5,key_ciphertext=$6,parameters=$7,enabled=$8,is_default=$9,max_calls=$10,timeout_seconds=$11,revision=revision+1 WHERE id=$1")
    } else {
        sqlx::query("INSERT INTO model_profiles(id,name,api,base_url,model,key_ciphertext,parameters,enabled,is_default,max_calls,timeout_seconds) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
    }.bind(id).bind(input.name).bind(input.api).bind(input.base_url.trim_end_matches('/')).bind(input.model).bind(ciphertext).bind(input.parameters).bind(input.enabled).bind(input.is_default).bind(input.max_calls).bind(input.timeout_seconds)
        .execute(&mut *tx).await?;
    if affected.rows_affected() != 1 {
        return Err(ApiError::NotFound);
    }
    tx.commit().await?;
    Ok(())
}
pub async fn delete_profile(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    sqlx::query("DELETE FROM model_profiles WHERE id=$1")
        .bind(id)
        .execute(&state.pool)
        .await?;
    state
        .audit(auth.user_id, "model_profile_deleted", id, json!({}))
        .await?;
    Ok(Json(json!({"ok":true})))
}

pub async fn model_snapshot(state: &AppState, id: Option<Uuid>) -> anyhow::Result<Option<Value>> {
    Ok(sqlx::query_scalar("SELECT to_jsonb(m) FROM model_profiles m WHERE enabled AND (($1::uuid IS NOT NULL AND id=$1) OR ($1::uuid IS NULL AND is_default)) LIMIT 1")
        .bind(id).fetch_optional(&state.pool).await?)
}

pub async fn workers(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Value>>> {
    auth.session_role("admin")?;
    Ok(Json(sqlx::query_scalar("SELECT (to_jsonb(w)-'certificate_hash') || jsonb_build_object('online',last_seen>now()-interval '30 seconds','active_run',(SELECT id FROM planning_runs r WHERE r.worker_id=w.id AND r.state='leased' LIMIT 1)) FROM ai_workers w ORDER BY created_at")
        .fetch_all(&state.pool).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerUpdate {
    draining: bool,
    revoked: bool,
}
pub async fn update_worker(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(input): Json<WorkerUpdate>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    let mut tx = state.pool.begin().await?;
    let revoked: bool = sqlx::query_scalar(
        "UPDATE ai_workers SET draining=$2,revoked=revoked OR $3 WHERE id=$1 RETURNING revoked",
    )
    .bind(id)
    .bind(input.draining)
    .bind(input.revoked)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    tx.commit().await?;
    state
        .audit(
            auth.user_id,
            "ai_worker_updated",
            id,
            json!({"draining":input.draining,"revoked":revoked}),
        )
        .await?;
    Ok(Json(json!({"ok":true})))
}
pub async fn enrollment(State(state): State<AppState>, auth: Auth) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    let id = Uuid::new_v4();
    let token = crypto::token();
    sqlx::query("INSERT INTO ai_enrollment_tokens(id,token_hash,expires_at) VALUES($1,$2,now()+interval '15 minutes')").bind(id).bind(crypto::hash(&token)).execute(&state.pool).await?;
    state
        .audit(auth.user_id, "ai_enrollment_created", id, json!({}))
        .await?;
    Ok(Json(json!({"id":id,"token":token,"expires_in":900})))
}
pub async fn source_metrics(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Value>>> {
    auth.session_role("admin")?;
    Ok(Json(sqlx::query_scalar("SELECT to_jsonb(s)-'cache_key' FROM source_repositories s ORDER BY last_used_at DESC LIMIT 100").fetch_all(&state.pool).await?))
}

pub async fn clarify(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(input): Json<Value>,
) -> Result<Json<Value>> {
    let answer = input
        .get("answer")
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty() && v.len() <= 8000)
        .ok_or_else(|| ApiError::BadRequest("answer is required".into()))?;
    let mut tx = state.pool.begin().await?;
    let row=sqlx::query("SELECT project_id,status,cancel_requested,planning_context,clarification FROM builds WHERE id=$1 FOR UPDATE").bind(id).fetch_one(&mut *tx).await?;
    auth.project(row.get("project_id"), "build")?;
    if row.get::<String, _>("status") != "needs_input" || row.get::<bool, _>("cancel_requested") {
        return Err(ApiError::Conflict("build is not waiting for input".into()));
    }
    let previous=sqlx::query("SELECT model_config,failed_targets FROM planning_runs WHERE build_id=$1 ORDER BY created_at DESC LIMIT 1").bind(id).fetch_one(&mut *tx).await?;
    let mut context: Value = row.get("planning_context");
    if !context["clarifications"].is_array() {
        context["clarifications"] = json!([]);
    }
    context["clarifications"]
        .as_array_mut()
        .unwrap()
        .push(json!({"question":row.get::<Option<Value>,_>("clarification"),"answer":answer}));
    sqlx::query("INSERT INTO planning_runs(id,build_id,purpose,model_config,failed_targets) VALUES($1,$2,'clarification',$3,$4)").bind(Uuid::new_v4()).bind(id).bind(previous.get::<Value,_>("model_config")).bind(previous.get::<Vec<String>,_>("failed_targets")).execute(&mut *tx).await?;
    sqlx::query("UPDATE builds SET planning_context=$2,clarification=NULL,status=CASE WHEN current_plan_id IS NULL THEN 'planning' ELSE 'replanning' END WHERE id=$1").bind(id).bind(context).execute(&mut *tx).await?;
    tx.commit().await?;
    state
        .audit(auth.user_id, "build_clarified", id, json!({}))
        .await?;
    Ok(Json(json!({"ok":true})))
}
pub async fn plans(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let project: Uuid = sqlx::query_scalar("SELECT project_id FROM builds WHERE id=$1")
        .bind(id)
        .fetch_one(&state.pool)
        .await?;
    auth.project(project, "read")?;
    let plans: Vec<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(p) FROM plan_revisions p WHERE build_id=$1 ORDER BY revision",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await?;
    let runs:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(r)-'model_config'-'lease_hash' FROM planning_runs r WHERE build_id=$1 ORDER BY created_at").bind(id).fetch_all(&state.pool).await?;
    let events:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(e) FROM planning_events e JOIN planning_runs r ON r.id=e.run_id WHERE r.build_id=$1 AND e.kind<>'checkpoint' ORDER BY e.id DESC LIMIT 300").bind(id).fetch_all(&state.pool).await?;
    Ok(Json(json!({"plans":plans,"runs":runs,"events":events})))
}
