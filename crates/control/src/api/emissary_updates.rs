pub use crate::store::Target;
use crate::store::dto;
use crate::{
    auth::Auth,
    error::{ApiError, Result},
    ota,
    state::AppState,
};
use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

pub async fn releases(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<dto::EmissaryRelease>>> {
    auth.session_role("viewer")?;
    Ok(Json(state.store.emissary_releases().await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Import {
    tag: String,
}
pub async fn import(
    State(state): State<AppState>,
    auth: Auth,
    Json(input): Json<Import>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    let id = ota::import_tag(&state, &input.tag).await.map_err(|e| {
        tracing::warn!(error=%e,"Emissary release import rejected");
        ApiError::BadRequest(format!("Emissary release import rejected: {e}"))
    })?;
    state
        .audit(
            auth.user_id,
            "emissary_release_imported",
            id,
            json!({"tag":input.tag}),
        )
        .await?;
    Ok(Json(json!({"id":id})))
}

pub async fn node_target(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(input): Json<Target>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    state
        .store
        .set_node_target(id, &input, auth.user_id)
        .await?;
    Ok(Json(json!({"ok":true})))
}

pub async fn pool_target(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(input): Json<Target>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    if input.concurrency.is_some_and(|n| !(1..=32).contains(&n)) {
        return Err(ApiError::BadRequest("concurrency must be 1 to 32".into()));
    }
    state
        .store
        .set_pool_target(id, &input, auth.user_id)
        .await?;
    Ok(Json(json!({"ok":true})))
}

pub async fn updates(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<dto::EmissaryUpdate>>> {
    auth.session_role("viewer")?;
    Ok(Json(state.store.emissary_updates().await?))
}
pub async fn retry(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    let new_id = state.store.retry_update(id, auth.user_id).await?;
    Ok(Json(json!({"id":new_id})))
}

pub async fn cancel(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    state.store.cancel_update(id, auth.user_id).await?;
    Ok(Json(json!({"ok":true})))
}
