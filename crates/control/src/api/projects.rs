use crate::{
    auth::Auth,
    error::{ApiError, Result},
    source, source_cache,
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

pub async fn refs(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<source_cache::RepositoryRefs>> {
    auth.project(id, "read")?;
    let project = sqlx::query(
        "SELECT repository_url,credential_id,planner_backend FROM projects WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await?;
    if project.get::<String, _>("planner_backend") == "kubernetes" {
        let result = crate::runtime_bridge::remote_refs(
            &state,
            id,
            &project.get::<String, _>("repository_url"),
            project.get("credential_id"),
        )
        .await
        .map_err(|_| ApiError::BadGateway("Source Service reference query failed".into()))?;
        return Ok(Json(
            serde_json::from_value(result).map_err(anyhow::Error::from)?,
        ));
    }
    if let Some(client) = crate::local_source::client(&state).await? {
        return Ok(Json(
            client
                .post(
                    "/internal/local-source",
                    &json!({"operation":"refs","project_id":id}),
                )
                .await?,
        ));
    }
    source_cache::list_refs(
        &state,
        &project.get::<String, _>("repository_url"),
        project.get("credential_id"),
    )
    .await
    .map(Json)
    .map_err(|error| match error {
        source_cache::RefQueryError::Timeout => ApiError::GatewayTimeout(error.to_string()),
        _ => ApiError::BadGateway(error.to_string()),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectInput {
    #[serde(default)]
    planner_backend: Option<String>,
    name: String,
    repository_url: String,
    credential_id: Option<Uuid>,
    #[serde(default = "default_pools")]
    pool_ids: Vec<Uuid>,
    #[serde(default)]
    signing_profiles: Vec<String>,
    #[serde(default)]
    model_profile_id: Option<Uuid>,
    #[serde(default)]
    build_context: String,
    #[serde(default)]
    source_credentials: Vec<crate::source_cache::CredentialBinding>,
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
        || input.build_context.len() > 16000
        || input.source_credentials.len() > 128
    {
        return Err(ApiError::BadRequest("invalid project settings".into()));
    }
    if let Some(backend) = &input.planner_backend
        && (!matches!(backend.as_str(), "poll" | "kubernetes")
            || (backend == "kubernetes" && std::env::var("ARTIFACTORY_RUNTIME_URL").is_err()))
    {
        return Err(ApiError::BadRequest("planner backend unavailable".into()));
    }
    source::validate_repository(&input.repository_url)
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    for binding in &input.source_credentials {
        source::validate_repository(&binding.repository_url)
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    }
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
    sqlx::query("INSERT INTO projects(id,name,repository_url,credential_id,pool_ids,signing_profiles,model_profile_id,build_context,source_credentials,planner_backend) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)").bind(id).bind(input.name).bind(input.repository_url).bind(input.credential_id).bind(input.pool_ids).bind(input.signing_profiles).bind(input.model_profile_id).bind(input.build_context).bind(serde_json::to_value(input.source_credentials).map_err(anyhow::Error::from)?).bind(input.planner_backend.unwrap_or("poll".into())).execute(&state.pool).await?;
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
    sqlx::query("UPDATE projects SET name=$2,repository_url=$3,credential_id=$4,pool_ids=$5,signing_profiles=$6,model_profile_id=$7,build_context=$8,source_credentials=$9,planner_backend=coalesce($10,planner_backend) WHERE id=$1").bind(id).bind(input.name).bind(input.repository_url).bind(input.credential_id).bind(input.pool_ids).bind(input.signing_profiles).bind(input.model_profile_id).bind(input.build_context).bind(serde_json::to_value(input.source_credentials).map_err(anyhow::Error::from)?).bind(input.planner_backend).execute(&state.pool).await?;
    state
        .audit(auth.user_id, "project_updated", id, json!({}))
        .await?;
    Ok(Json(json!({"id":id})))
}
