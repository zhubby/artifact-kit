use crate::store::dto;
use crate::{
    auth::Auth,
    error::{ApiError, Result},
    scheduler,
    state::AppState,
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{
        IntoResponse, Redirect, Sse,
        sse::{Event, KeepAlive},
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{convert::Infallible, time::Duration};
use uuid::Uuid;

pub use crate::store::BuildInput;

pub async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Path(project): Path<Uuid>,
    Json(mut input): Json<BuildInput>,
) -> Result<Json<Value>> {
    auth.project(project, "build")?;
    if input.release_version.is_some() {
        auth.project(project, "release")?;
    }
    if input.reference.is_empty()
        || input.reference.len() > 255
        || input.reference.starts_with('-')
        || input.reference.contains(['\0', '\n', '\r'])
        || !input.parameters.is_object()
        || input.parameters.to_string().len() > 16384
        || input.targets.len() > 32
    {
        return Err(ApiError::BadRequest("invalid build request".into()));
    }
    if let Some(version) = &input.release_version {
        crate::build::validate_release_version(version)?;
    }
    let goal = input.goal.take().unwrap_or_else(|| {
        "Build and verify this project, then collect its distributable artifacts.".into()
    });
    if goal.trim().is_empty() || goal.len() > 16000 {
        return Err(ApiError::BadRequest(
            "a build goal of 1..16000 bytes is required".into(),
        ));
    }
    let requirements = input
        .target_requirements
        .as_array()
        .ok_or_else(|| ApiError::BadRequest("target_requirements must be an array".into()))?;
    if requirements.len() > 32
        || requirements.iter().any(|r| {
            !r.is_object()
                || r.as_object()
                    .unwrap()
                    .keys()
                    .any(|k| k != "os" && k != "arch")
                || !["linux", "darwin", "windows"].contains(&r["os"].as_str().unwrap_or(""))
                || !["amd64", "arm64"].contains(&r["arch"].as_str().unwrap_or(""))
        })
    {
        return Err(ApiError::BadRequest("invalid platform requirements".into()));
    }
    let id = state
        .store
        .create_build(project, auth.user_id, &input, goal)
        .await?;
    state
        .audit(
            auth.user_id,
            "build_created",
            id,
            json!({"project_id":project,"ref":input.reference}),
        )
        .await?;
    Ok(Json(json!({"id":id})))
}

#[derive(Deserialize, Default)]
pub struct Filter {
    pub project_id: Option<Uuid>,
}

fn read_scope(auth: &Auth) -> Result<()> {
    if auth.token_projects.is_some() && !auth.token_scopes.iter().any(|scope| scope == "read") {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

pub async fn list(
    State(state): State<AppState>,
    auth: Auth,
    Query(filter): Query<Filter>,
) -> Result<Json<Vec<dto::BuildSummary>>> {
    read_scope(&auth)?;
    if let Some(project) = filter.project_id {
        auth.project(project, "read")?;
    }
    Ok(Json(
        state
            .store
            .builds(filter.project_id, auth.visible_projects())
            .await?,
    ))
}

pub async fn build_detail(state: &AppState, id: Uuid) -> Result<Value> {
    let mut build =
        serde_json::to_value(state.store.build_detail(id).await?).map_err(anyhow::Error::from)?;
    build["source_snapshot"] = if let Some(snapshot) = build["source_snapshot_id"].as_str() {
        let id = Uuid::parse_str(snapshot).map_err(anyhow::Error::from)?;
        serde_json::to_value(crate::source_cache::load(state, id).await?)
            .map_err(anyhow::Error::from)?
    } else {
        Value::Null
    };
    Ok(build)
}

pub async fn detail(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    auth.project(state.store.build_project(id).await?, "read")?;
    Ok(Json(build_detail(&state, id).await?))
}

pub async fn cancel(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    auth.project(state.store.build_project(id).await?, "build")?;
    state.store.cancel_build(id).await?;
    state
        .audit(auth.user_id, "build_canceled", id, json!({}))
        .await?;
    Ok(Json(json!({"ok":true})))
}

pub async fn retry(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    state.store.retry_job(id, &auth).await?;
    state
        .audit(auth.user_id, "job_retry_requested", id, json!({}))
        .await?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishInput {
    version: Option<String>,
}

pub async fn publish(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(input): Json<PublishInput>,
) -> Result<Json<Value>> {
    auth.project(state.store.build_project(id).await?, "release")?;
    state.store.request_publication(id, input.version).await?;
    let release = scheduler::publish_build(&state, id)
        .await
        .map_err(|error| ApiError::Conflict(error.to_string()))?;
    state
        .audit(
            auth.user_id,
            "publish_requested",
            id,
            json!({"release_id":release}),
        )
        .await?;
    Ok(Json(json!({"id":release})))
}

pub async fn artifacts(
    State(state): State<AppState>,
    auth: Auth,
    Query(filter): Query<Filter>,
) -> Result<Json<Vec<dto::ArtifactSummary>>> {
    read_scope(&auth)?;
    if let Some(project) = filter.project_id {
        auth.project(project, "read")?;
    }
    Ok(Json(
        state
            .store
            .artifacts(filter.project_id, auth.visible_projects())
            .await?,
    ))
}

pub async fn download(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Redirect> {
    let artifact = state.store.artifact_download(id).await?;
    auth.project(artifact.project_id, "read")?;
    state
        .audit(auth.user_id, "artifact_downloaded", id, json!({}))
        .await?;
    Ok(Redirect::temporary(
        &state.storage.download_url(&artifact.object_key),
    ))
}

pub async fn releases(
    State(state): State<AppState>,
    auth: Auth,
    Query(filter): Query<Filter>,
) -> Result<Json<Vec<dto::ReleaseSummary>>> {
    read_scope(&auth)?;
    if let Some(project) = filter.project_id {
        auth.project(project, "read")?;
    }
    Ok(Json(
        state
            .store
            .releases(filter.project_id, auth.visible_projects())
            .await?,
    ))
}

pub async fn release_detail(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let mut release =
        serde_json::to_value(state.store.release(id).await?).map_err(anyhow::Error::from)?;
    auth.project(
        Uuid::parse_str(release["project_id"].as_str().unwrap()).unwrap(),
        "read",
    )?;
    release["artifacts"] = json!(state.store.release_artifacts(id).await?);
    Ok(Json(release))
}

#[derive(Deserialize, Default)]
pub struct EventQuery {
    #[serde(default)]
    after: i64,
}

pub async fn events(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Query(query): Query<EventQuery>,
    headers: HeaderMap,
) -> Result<impl IntoResponse> {
    auth.project(state.store.build_project(id).await?, "read")?;
    let mut cursor = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(query.after)
        .max(0);
    let stream = async_stream::stream! {
        let started=tokio::time::Instant::now();
        loop {
            if let Ok(snapshot)=build_detail(&state,id).await {yield Ok::<Event,Infallible>(Event::default().event("snapshot").data(snapshot.to_string()));}
            let logs = state.store.log_chunks(id, cursor).await;
            if let Ok(logs)=logs {
                for log in logs {
                    let text=match state.storage.get_log(&log.object_key).await {Ok(bytes)=>String::from_utf8_lossy(&bytes).into_owned(),Err(_)=>break};
                    cursor=log.id;
                    yield Ok(Event::default().id(cursor.to_string()).event("log").data(json!({"id":cursor,"attempt_id":log.attempt_id,"step":log.step,"text":text}).to_string()));
                }
            }
            if started.elapsed()>Duration::from_secs(55) {break;}
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    };
    Ok((
        [
            ("cache-control", "no-cache, no-transform"),
            ("x-accel-buffering", "no"),
        ],
        Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))),
    ))
}
