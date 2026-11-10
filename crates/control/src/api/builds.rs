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
use sqlx::Row;
use std::{convert::Infallible, time::Duration};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildInput {
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default = "empty_object")]
    parameters: Value,
    #[serde(default)]
    targets: Vec<String>,
    release_version: Option<String>,
}
fn empty_object() -> Value {
    json!({})
}

pub async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Path(project): Path<Uuid>,
    Json(input): Json<BuildInput>,
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
        validate_version(version)?;
    }
    let id = Uuid::new_v4();
    let created=sqlx::query("INSERT INTO builds(id,project_id,requested_ref,parameters,selected_targets,release_version,publish_status,requested_by,source_repository_url,source_credential_id) SELECT $1,id,$3,$4,$5,$6,$7,$8,repository_url,credential_id FROM projects WHERE id=$2")
        .bind(id).bind(project).bind(&input.reference).bind(input.parameters).bind(input.targets).bind(&input.release_version).bind(if input.release_version.is_some(){"pending"}else{"not_requested"}).bind(auth.user_id).execute(&state.pool).await?;
    if created.rows_affected() != 1 {
        return Err(ApiError::NotFound);
    }
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

fn validate_version(version: &str) -> Result<()> {
    if version.is_empty() || version.len() > 100 || version.chars().any(char::is_control) {
        return Err(ApiError::BadRequest("invalid release version".into()));
    }
    Ok(())
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
) -> Result<Json<Vec<Value>>> {
    read_scope(&auth)?;
    if let Some(project) = filter.project_id {
        auth.project(project, "read")?;
    }
    Ok(Json(sqlx::query_scalar("SELECT (to_jsonb(b)-'config_yaml'-'config_snapshot') || jsonb_build_object('project_name',p.name,'targets',(SELECT coalesce(jsonb_agg(jsonb_build_object('id',j.id,'target',j.target,'status',j.status)),'[]') FROM jobs j WHERE j.build_id=b.id)) FROM builds b JOIN projects p ON p.id=b.project_id WHERE ($1::uuid IS NULL OR b.project_id=$1) AND ($2::uuid[] IS NULL OR b.project_id=ANY($2)) ORDER BY b.created_at DESC LIMIT 100")
        .bind(filter.project_id).bind(auth.visible_projects()).fetch_all(&state.pool).await?))
}

async fn build_project(state: &AppState, id: Uuid) -> Result<Uuid> {
    Ok(
        sqlx::query_scalar("SELECT project_id FROM builds WHERE id=$1")
            .bind(id)
            .fetch_one(&state.pool)
            .await?,
    )
}

pub async fn build_detail(state: &AppState, id: Uuid) -> Result<Value> {
    let mut build:Value=sqlx::query_scalar("SELECT to_jsonb(b) || jsonb_build_object('project_name',p.name) FROM builds b JOIN projects p ON p.id=b.project_id WHERE b.id=$1").bind(id).fetch_one(&state.pool).await?;
    let jobs:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(j) || jsonb_build_object('attempts',(SELECT coalesce(jsonb_agg((to_jsonb(a)-'lease_token'-'command_id') || jsonb_build_object('node_name',n.name,'steps',(SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY s.step_index),'[]') FROM step_runs s WHERE s.attempt_id=a.id)) ORDER BY a.created_at),'[]') FROM attempts a JOIN nodes n ON n.id=a.node_id WHERE a.job_id=j.id)) FROM jobs j WHERE j.build_id=$1 ORDER BY j.target").bind(id).fetch_all(&state.pool).await?;
    let artifacts:Vec<Value>=sqlx::query_scalar("SELECT (to_jsonb(ar)-'object_key') || jsonb_build_object('target',j.target,'attempt_status',a.status) FROM artifacts ar JOIN attempts a ON a.id=ar.attempt_id JOIN jobs j ON j.id=a.job_id WHERE j.build_id=$1 ORDER BY ar.created_at").bind(id).fetch_all(&state.pool).await?;
    build["jobs"] = json!(jobs);
    build["artifacts"] = json!(artifacts);
    build["publish_attempts"] = json!(
        sqlx::query_scalar::<_, Value>(
            "SELECT to_jsonb(p) FROM publish_attempts p WHERE build_id=$1 ORDER BY created_at DESC"
        )
        .bind(id)
        .fetch_all(&state.pool)
        .await?
    );
    Ok(build)
}

pub async fn detail(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    auth.project(build_project(&state, id).await?, "read")?;
    Ok(Json(build_detail(&state, id).await?))
}

pub async fn cancel(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    auth.project(build_project(&state, id).await?, "build")?;
    let mut transaction = state.pool.begin().await?;
    let status: String = sqlx::query_scalar("SELECT status FROM builds WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *transaction)
        .await?;
    if ["succeeded", "failed", "lost", "canceled"].contains(&status.as_str()) {
        return Err(ApiError::Conflict("build is already terminal".into()));
    }
    sqlx::query("UPDATE builds SET cancel_requested=true,status='canceling' WHERE id=$1")
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("UPDATE jobs SET status='canceled' WHERE build_id=$1 AND status='queued'")
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    if status == "resolving" {
        sqlx::query("UPDATE builds SET status='canceled',finished_at=now() WHERE id=$1")
            .bind(id)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
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
    let mut transaction = state.pool.begin().await?;
    let row=sqlx::query("SELECT j.status,b.id,b.project_id,b.release_version,EXISTS(SELECT 1 FROM releases r WHERE r.build_id=b.id) AS published FROM jobs j JOIN builds b ON b.id=j.build_id WHERE j.id=$1 FOR UPDATE OF j,b").bind(id).fetch_one(&mut *transaction).await?;
    auth.project(row.get("project_id"), "build")?;
    if row.get::<Option<String>, _>("release_version").is_some() {
        auth.project(row.get("project_id"), "release")?;
    }
    if row.get::<bool, _>("published")
        || !["failed", "lost", "canceled"].contains(&row.get::<String, _>("status").as_str())
    {
        return Err(ApiError::Conflict(
            "only failed, lost or canceled unpublished jobs can be retried".into(),
        ));
    }
    sqlx::query("UPDATE jobs SET status='queued' WHERE id=$1")
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("UPDATE builds SET status='queued',cancel_requested=false,finished_at=NULL,publish_status=CASE WHEN release_version IS NULL THEN 'not_requested' ELSE 'pending' END,publish_error=NULL WHERE id=$1").bind(row.get::<Uuid,_>("id")).execute(&mut *transaction).await?;
    transaction.commit().await?;
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
    auth.project(build_project(&state, id).await?, "release")?;
    let mut transaction = state.pool.begin().await?;
    let row = sqlx::query("SELECT status,release_version FROM builds WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *transaction)
        .await?;
    if row.get::<String, _>("status") != "succeeded" {
        return Err(ApiError::Conflict(
            "build must succeed before publication".into(),
        ));
    }
    let current: Option<String> = row.get("release_version");
    if let Some(version) = input.version {
        validate_version(&version)?;
        if current.as_ref().is_some_and(|current| current != &version) {
            return Err(ApiError::Conflict(
                "release version is immutable for this build".into(),
            ));
        }
        sqlx::query("UPDATE builds SET release_version=$2 WHERE id=$1")
            .bind(id)
            .bind(version)
            .execute(&mut *transaction)
            .await?;
    } else if current.is_none() {
        return Err(ApiError::BadRequest("version is required".into()));
    }
    transaction.commit().await?;
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
) -> Result<Json<Vec<Value>>> {
    read_scope(&auth)?;
    if let Some(project) = filter.project_id {
        auth.project(project, "read")?;
    }
    Ok(Json(sqlx::query_scalar("SELECT (to_jsonb(ar)-'object_key') || jsonb_build_object('target',j.target,'build_id',b.id,'project_name',p.name,'project_id',p.id) FROM artifacts ar JOIN attempts a ON a.id=ar.attempt_id JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id JOIN projects p ON p.id=b.project_id WHERE ($1::uuid IS NULL OR p.id=$1) AND ($2::uuid[] IS NULL OR p.id=ANY($2)) ORDER BY ar.created_at DESC LIMIT 100")
        .bind(filter.project_id).bind(auth.visible_projects()).fetch_all(&state.pool).await?))
}

pub async fn download(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Redirect> {
    let row=sqlx::query("SELECT ar.object_key,b.project_id FROM artifacts ar JOIN attempts a ON a.id=ar.attempt_id JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE ar.id=$1").bind(id).fetch_one(&state.pool).await?;
    auth.project(row.get("project_id"), "read")?;
    state
        .audit(auth.user_id, "artifact_downloaded", id, json!({}))
        .await?;
    Ok(Redirect::temporary(
        &state
            .storage
            .download_url(&row.get::<String, _>("object_key")),
    ))
}

pub async fn releases(
    State(state): State<AppState>,
    auth: Auth,
    Query(filter): Query<Filter>,
) -> Result<Json<Vec<Value>>> {
    read_scope(&auth)?;
    if let Some(project) = filter.project_id {
        auth.project(project, "read")?;
    }
    Ok(Json(sqlx::query_scalar("SELECT to_jsonb(r) || jsonb_build_object('project_name',p.name,'artifact_count',(SELECT count(*) FROM release_artifacts ra WHERE ra.release_id=r.id)) FROM releases r JOIN projects p ON p.id=r.project_id WHERE ($1::uuid IS NULL OR p.id=$1) AND ($2::uuid[] IS NULL OR p.id=ANY($2)) ORDER BY r.created_at DESC LIMIT 100")
        .bind(filter.project_id).bind(auth.visible_projects()).fetch_all(&state.pool).await?))
}

pub async fn release_detail(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let mut release:Value=sqlx::query_scalar("SELECT to_jsonb(r) || jsonb_build_object('project_name',p.name) FROM releases r JOIN projects p ON p.id=r.project_id WHERE r.id=$1").bind(id).fetch_one(&state.pool).await?;
    auth.project(
        Uuid::parse_str(release["project_id"].as_str().unwrap()).unwrap(),
        "read",
    )?;
    release["artifacts"]=json!(sqlx::query_scalar::<_,Value>("SELECT (to_jsonb(ar)-'object_key') || jsonb_build_object('target',j.target) FROM release_artifacts ra JOIN artifacts ar ON ar.id=ra.artifact_id JOIN attempts a ON a.id=ar.attempt_id JOIN jobs j ON j.id=a.job_id WHERE ra.release_id=$1 ORDER BY j.target,ar.path").bind(id).fetch_all(&state.pool).await?);
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
    auth.project(build_project(&state, id).await?, "read")?;
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
            let logs=sqlx::query("SELECT l.id,l.attempt_id,l.step,l.object_key FROM log_chunks l JOIN attempts a ON a.id=l.attempt_id JOIN jobs j ON j.id=a.job_id WHERE j.build_id=$1 AND l.id>$2 ORDER BY l.id LIMIT 100").bind(id).bind(cursor).fetch_all(&state.pool).await;
            if let Ok(logs)=logs {
                for log in logs {
                    let text=match state.storage.get_log(&log.get::<String,_>("object_key")).await {Ok(bytes)=>String::from_utf8_lossy(&bytes).into_owned(),Err(_)=>break};
                    cursor=log.get("id");
                    yield Ok(Event::default().id(cursor.to_string()).event("log").data(json!({"id":cursor,"attempt_id":log.get::<Uuid,_>("attempt_id"),"step":log.get::<String,_>("step"),"text":text}).to_string()));
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
