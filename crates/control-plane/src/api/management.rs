use crate::store::dto;
pub use crate::store::{NewToken, NodeUpdate};
use crate::{
    auth::{self, Auth},
    crypto,
    error::{ApiError, Result},
    source::GitCredential,
    state::AppState,
};
use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

pub async fn overview(State(state): State<AppState>, auth: Auth) -> Result<Json<dto::Overview>> {
    auth.session_role("viewer")?;
    let value = state.store.overview().await?;
    Ok(Json(value))
}

pub async fn users(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<dto::UserSummary>>> {
    auth.session_role("admin")?;
    Ok(Json(state.store.users().await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewUser {
    username: String,
    password: String,
    role: String,
}

pub async fn create_user(
    State(state): State<AppState>,
    auth: Auth,
    Json(input): Json<NewUser>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    if input.username.is_empty()
        || input.username.len() > 100
        || !["admin", "operator", "viewer"].contains(&input.role.as_str())
    {
        return Err(ApiError::BadRequest("invalid user".into()));
    }
    let password = auth::password_hash(&input.password)
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    let id = Uuid::new_v4();
    state
        .store
        .create_user(id, &input.username, &password, &input.role)
        .await?;
    state
        .audit(auth.user_id, "user_created", id, json!({}))
        .await?;
    Ok(Json(json!({"id":id})))
}

pub async fn tokens(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<dto::TokenSummary>>> {
    auth.session_role("viewer")?;
    Ok(Json(state.store.tokens(auth.user_id).await?))
}

pub async fn create_token(
    State(state): State<AppState>,
    auth: Auth,
    Json(input): Json<NewToken>,
) -> Result<Json<Value>> {
    auth.session_role("viewer")?;
    if input.name.is_empty()
        || input.name.len() > 100
        || input.project_ids.is_empty()
        || input.scopes.is_empty()
        || !(1..=365).contains(&input.days)
        || input
            .scopes
            .iter()
            .any(|scope| !["read", "build", "release"].contains(&scope.as_str()))
    {
        return Err(ApiError::BadRequest("invalid token scope or expiry".into()));
    }
    for project in &input.project_ids {
        for scope in &input.scopes {
            auth.project(*project, scope)?;
        }
        let exists: bool = state.store.project_exists(*project).await?;
        if !exists {
            return Err(ApiError::NotFound);
        }
    }
    let id = Uuid::new_v4();
    let token = format!("artifactory_{}", crypto::token());
    state
        .store
        .create_token(id, auth.user_id, input, crypto::hash(&token))
        .await?;
    state
        .audit(auth.user_id, "api_token_created", id, json!({}))
        .await?;
    Ok(Json(json!({"id":id,"token":token})))
}

pub async fn revoke_token(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    auth.session_role("viewer")?;
    state.store.revoke_token(id, auth.user_id).await?;
    state
        .audit(auth.user_id, "api_token_revoked", id, json!({}))
        .await?;
    Ok(Json(json!({"ok":true})))
}

pub async fn pools(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<dto::PoolSummary>>> {
    auth.session_role("viewer")?;
    Ok(Json(state.store.pools().await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Name {
    name: String,
}

pub async fn create_pool(
    State(state): State<AppState>,
    auth: Auth,
    Json(input): Json<Name>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    if input.name.is_empty() || input.name.len() > 100 {
        return Err(ApiError::BadRequest("invalid pool name".into()));
    }
    let id = Uuid::new_v4();
    state.store.create_pool(id, &input.name).await?;
    state
        .audit(auth.user_id, "pool_created", id, json!({}))
        .await?;
    Ok(Json(json!({"id":id})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewEnrollment {
    pool_id: Uuid,
}

pub async fn enrollment_token(
    State(state): State<AppState>,
    auth: Auth,
    Json(input): Json<NewEnrollment>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    let id = Uuid::new_v4();
    let token = crypto::token();
    state
        .store
        .enrollment_token(id, crypto::hash(&token), input.pool_id)
        .await?;
    state
        .audit(
            auth.user_id,
            "enrollment_token_created",
            id,
            json!({"pool_id":input.pool_id}),
        )
        .await?;
    Ok(Json(json!({"id":id,"token":token,"expires_seconds":900})))
}

#[derive(Debug, Deserialize)]
pub struct NodeQuery {
    #[serde(default = "node_page_size")]
    limit: i64,
    #[serde(default)]
    offset: i64,
    #[serde(default)]
    q: String,
}
fn node_page_size() -> i64 {
    100
}

pub async fn nodes(
    State(state): State<AppState>,
    auth: Auth,
    Query(query): Query<NodeQuery>,
) -> Result<Json<Vec<dto::NodeSummary>>> {
    auth.session_role("viewer")?;
    if !(1..=500).contains(&query.limit)
        || !(0..=10_000_000).contains(&query.offset)
        || query.q.len() > 100
    {
        return Err(ApiError::BadRequest(
            "invalid node pagination or search".into(),
        ));
    }
    Ok(Json(
        state
            .store
            .nodes(query.limit, query.offset, &query.q)
            .await?,
    ))
}

pub async fn update_node(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(input): Json<NodeUpdate>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    if !(1..=32).contains(&input.slots)
        || !input.labels.is_object()
        || input.labels.to_string().len() > 8192
        || input.signing_profiles.len() > 32
    {
        return Err(ApiError::BadRequest("invalid node settings".into()));
    }
    let revoked = state.store.update_node(id, &input).await?;
    state
        .audit(
            auth.user_id,
            "node_updated",
            id,
            json!({"draining":input.draining,"revoked":revoked}),
        )
        .await?;
    Ok(Json(json!({"ok":true})))
}

pub async fn credentials(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<dto::CredentialSummary>>> {
    auth.session_role("admin")?;
    Ok(Json(state.store.credentials().await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewCredential {
    name: String,
    kind: String,
    #[serde(default)]
    username: String,
    secret: String,
    #[serde(default)]
    known_hosts: String,
}

pub async fn create_credential(
    State(state): State<AppState>,
    actor: Auth,
    Json(input): Json<NewCredential>,
) -> Result<Json<Value>> {
    actor.session_role("admin")?;
    let input_name = input.name;
    let input = GitCredential {
        kind: input.kind,
        username: input.username,
        secret: input.secret,
        known_hosts: input.known_hosts,
    };
    if input_name.is_empty()
        || input_name.len() > 100
        || !["ssh", "https"].contains(&input.kind.as_str())
        || input.secret.is_empty()
        || input.secret.len() > 65536
        || (input.kind == "ssh" && input.known_hosts.trim().is_empty())
    {
        return Err(ApiError::BadRequest(
            "invalid credential; SSH requires known_hosts".into(),
        ));
    }
    let id = Uuid::new_v4();
    let encrypted = crypto::seal(
        &state.settings.master_key,
        &serde_json::to_vec(&input).map_err(anyhow::Error::from)?,
    )?;
    state
        .store
        .create_credential(id, input_name, input.kind, encrypted)
        .await?;
    state
        .audit(actor.user_id, "credential_created", id, json!({}))
        .await?;
    Ok(Json(json!({"id":id})))
}

pub async fn audit(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<dto::AuditEvent>>> {
    auth.session_role("admin")?;
    Ok(Json(state.store.audit_events().await?))
}
