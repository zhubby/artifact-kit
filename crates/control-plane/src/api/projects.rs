pub use crate::store::ProjectInput;
use crate::store::dto;
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
use serde_json::{Value, json};
use uuid::Uuid;

pub async fn list(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<dto::ProjectSummary>>> {
    if auth.token_projects.is_some() && !auth.token_scopes.iter().any(|scope| scope == "read") {
        return Err(ApiError::Forbidden);
    }
    Ok(Json(state.store.projects(auth.visible_projects()).await?))
}

pub async fn detail(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<dto::Project>> {
    auth.project(id, "read")?;
    Ok(Json(state.store.project(id).await?))
}

pub async fn refs(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<source_cache::RepositoryRefs>> {
    auth.project(id, "read")?;
    let project = state.store.project_source(id).await?;
    if project.planner_backend == "kubernetes" {
        let result = crate::runtime_bridge::remote_refs(
            &state,
            id,
            &project.repository_url,
            project.credential_id,
        )
        .await
        .map_err(|_| ApiError::BadGateway("Source Service reference query failed".into()))?;
        return Ok(Json(
            serde_json::from_value(result).map_err(anyhow::Error::from)?,
        ));
    }
    if let Some(client) = crate::local_source::client(&state).await? {
        let result = client
            .post(
                "/internal/local-source",
                &json!({"operation":"refs","project_id":id}),
            )
            .await?;
        return Ok(Json(
            serde_json::from_value(result).map_err(anyhow::Error::from)?,
        ));
    }
    source_cache::list_refs(&state, &project.repository_url, project.credential_id)
        .await
        .map(Json)
        .map_err(|error| match error {
            source_cache::RefQueryError::Timeout => ApiError::GatewayTimeout(error.to_string()),
            _ => ApiError::BadGateway(error.to_string()),
        })
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
    let count = state.store.pool_count(&input.pool_ids).await?;
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
    state.store.create_project(id, &input).await?;
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
    state.store.update_project(id, &input).await?;
    state
        .audit(auth.user_id, "project_updated", id, json!({}))
        .await?;
    Ok(Json(json!({"id":id})))
}
