use crate::{
    auth::Auth,
    error::{ApiError, Result},
    source,
    state::AppState,
};
use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

pub async fn list(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Value>>> {
    if auth.token_projects.is_some() && !auth.token_scopes.iter().any(|scope| scope == "read") {
        return Err(ApiError::Forbidden);
    }
    Ok(Json(sqlx::query_scalar("SELECT to_jsonb(p) || jsonb_build_object('build_count',(SELECT count(*) FROM builds b WHERE b.project_id=p.id),'last_build',(SELECT jsonb_build_object('id',b.id,'status',b.status,'created_at',b.created_at) FROM builds b WHERE b.project_id=p.id ORDER BY created_at DESC LIMIT 1)) FROM projects p WHERE ($1::uuid[] IS NULL OR p.id=ANY($1)) ORDER BY p.created_at DESC").bind(auth.visible_projects()).fetch_all(&state.pool).await?))
}

pub async fn detail(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    auth.project(id, "read")?;
    Ok(Json(
        sqlx::query_scalar("SELECT to_jsonb(p) FROM projects p WHERE id=$1")
            .bind(id)
            .fetch_one(&state.pool)
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectInput {
    name: String,
    repository_url: String,
    credential_id: Option<Uuid>,
    #[serde(default = "default_pools")]
    pool_ids: Vec<Uuid>,
    #[serde(default)]
    signing_profiles: Vec<String>,
}

fn default_pools() -> Vec<Uuid> {
    vec![Uuid::from_u128(1)]
}

async fn validate(state: &AppState, input: &ProjectInput) -> Result<()> {
    if input.name.is_empty()
        || input.name.len() > 100
        || input.pool_ids.is_empty()
        || input.pool_ids.len() > 32
        || input.signing_profiles.len() > 32
    {
        return Err(ApiError::BadRequest("invalid project settings".into()));
    }
    source::validate_repository(&input.repository_url)
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM node_pools WHERE id=ANY($1)")
        .bind(&input.pool_ids)
        .fetch_one(&state.pool)
        .await?;
    if count as usize != input.pool_ids.len() {
        return Err(ApiError::BadRequest(
            "unknown or duplicate node pool".into(),
        ));
    }
    Ok(())
}

pub async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Json(input): Json<ProjectInput>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    validate(&state, &input).await?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO projects(id,name,repository_url,credential_id,pool_ids,signing_profiles) VALUES($1,$2,$3,$4,$5,$6)").bind(id).bind(input.name).bind(input.repository_url).bind(input.credential_id).bind(input.pool_ids).bind(input.signing_profiles).execute(&state.pool).await?;
    state
        .audit(auth.user_id, "project_created", id, json!({}))
        .await?;
    Ok(Json(json!({"id":id})))
}

pub async fn update(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(input): Json<ProjectInput>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    validate(&state, &input).await?;
    sqlx::query("UPDATE projects SET name=$2,repository_url=$3,credential_id=$4,pool_ids=$5,signing_profiles=$6 WHERE id=$1").bind(id).bind(input.name).bind(input.repository_url).bind(input.credential_id).bind(input.pool_ids).bind(input.signing_profiles).execute(&state.pool).await?;
    state
        .audit(auth.user_id, "project_updated", id, json!({}))
        .await?;
    Ok(Json(json!({"id":id})))
}
