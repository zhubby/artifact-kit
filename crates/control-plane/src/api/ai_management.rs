use crate::store::dto;
pub use crate::store::{ProfileInput, WorkerUpdate};
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
use serde_json::{Value, json};
use uuid::Uuid;

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

pub async fn profiles(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<dto::ModelProfile>>> {
    auth.session_role("admin")?;
    Ok(Json(state.store.model_profiles().await?))
}

pub async fn model_options(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<dto::ModelOption>>> {
    auth.session_role("operator")?;
    Ok(Json(state.store.model_options().await?))
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
    state
        .store
        .save_model_profile(&state.settings.master_key, id, input, false)
        .await?;
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
    state
        .store
        .save_model_profile(&state.settings.master_key, id, input, true)
        .await?;
    state
        .audit(auth.user_id, "model_profile_updated", id, json!({}))
        .await?;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_profile(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    state.store.delete_model_profile(id).await?;
    state
        .audit(auth.user_id, "model_profile_deleted", id, json!({}))
        .await?;
    Ok(Json(json!({"ok":true})))
}

pub async fn workers(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<dto::WorkerSummary>>> {
    auth.session_role("admin")?;
    Ok(Json(state.store.workers().await?))
}

pub async fn update_worker(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(input): Json<WorkerUpdate>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    let revoked = state.store.update_worker(id, &input).await?;
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
    state
        .store
        .planner_enrollment(id, crypto::hash(&token))
        .await?;
    state
        .audit(auth.user_id, "ai_enrollment_created", id, json!({}))
        .await?;
    Ok(Json(json!({"id":id,"token":token,"expires_in":900})))
}
pub async fn source_metrics(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<dto::SourceRepositorySummary>>> {
    auth.session_role("admin")?;
    Ok(Json(state.store.source_metrics().await?))
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
    state.store.clarify_build(id, answer, &auth).await?;
    state
        .audit(auth.user_id, "build_clarified", id, json!({}))
        .await?;
    Ok(Json(json!({"ok":true})))
}
pub async fn plans(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<dto::BuildPlans>> {
    Ok(Json(state.store.plans(id, &auth).await?))
}
