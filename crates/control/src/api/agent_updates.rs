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
use sqlx::Row;
use uuid::Uuid;

pub async fn releases(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Value>>> {
    auth.session_role("viewer")?;
    Ok(Json(
        sqlx::query_scalar("SELECT to_jsonb(r) FROM agent_releases r ORDER BY created_at DESC")
            .fetch_all(&state.pool)
            .await?,
    ))
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
        tracing::warn!(error=%e,"Agent release import rejected");
        ApiError::BadRequest(format!("Agent release import rejected: {e}"))
    })?;
    state
        .audit(
            auth.user_id,
            "agent_release_imported",
            id,
            json!({"tag":input.tag}),
        )
        .await?;
    Ok(Json(json!({"id":id})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    release_id: Option<Uuid>,
    concurrency: Option<i32>,
    paused: Option<bool>,
}
pub async fn node_target(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(input): Json<Target>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    let mut tx = state.pool.begin().await?;
    let revoked: bool = sqlx::query_scalar("SELECT revoked FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    if revoked {
        return Err(ApiError::Conflict("revoked nodes cannot upgrade".into()));
    }
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM agent_updates WHERE node_id=$1 AND finished_at IS NULL)",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if busy {
        return Err(ApiError::Conflict(
            "an update is already active; cancel or wait for it first".into(),
        ));
    }
    sqlx::query("UPDATE nodes SET agent_target_release=$2 WHERE id=$1")
        .bind(id)
        .bind(input.release_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO audit_events(actor,action,resource_id,details) VALUES($1,'agent_node_target_changed',$2,$3)").bind(auth.user_id.to_string()).bind(id.to_string()).bind(json!({"release_id":input.release_id})).execute(&mut *tx).await?;
    tx.commit().await?;
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
    let mut tx = state.pool.begin().await?;
    let changed=sqlx::query("UPDATE node_pools SET agent_target_release=$2,agent_update_concurrency=coalesce($3,agent_update_concurrency),agent_updates_paused=coalesce($4,agent_updates_paused) WHERE id=$1").bind(id).bind(input.release_id).bind(input.concurrency).bind(input.paused).execute(&mut *tx).await?.rows_affected();
    if changed == 0 {
        return Err(ApiError::NotFound);
    }
    sqlx::query("INSERT INTO audit_events(actor,action,resource_id,details) VALUES($1,'agent_pool_policy_changed',$2,$3)").bind(auth.user_id.to_string()).bind(id.to_string()).bind(json!({"release_id":input.release_id,"concurrency":input.concurrency,"paused":input.paused})).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"ok":true})))
}
pub async fn updates(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Value>>> {
    auth.session_role("viewer")?;
    Ok(Json(sqlx::query_scalar("SELECT to_jsonb(u)||jsonb_build_object('version',r.version,'node_name',n.name) FROM agent_updates u JOIN agent_releases r ON r.id=u.release_id JOIN nodes n ON n.id=u.node_id ORDER BY u.created_at DESC LIMIT 500").fetch_all(&state.pool).await?))
}
pub async fn retry(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    let u = sqlx::query("SELECT * FROM agent_updates WHERE id=$1")
        .bind(id)
        .fetch_one(&state.pool)
        .await?;
    if !["failed", "rolled_back", "canceled"].contains(&u.get::<String, _>("status").as_str()) {
        return Err(ApiError::Conflict(
            "only a failed, rolled back or canceled update can be retried".into(),
        ));
    }
    let node: Uuid = u.get("node_id");
    let pool: Uuid = sqlx::query_scalar("SELECT pool_id FROM nodes WHERE id=$1")
        .bind(node)
        .fetch_one(&state.pool)
        .await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,57))")
        .bind(pool.to_string())
        .execute(&mut *tx)
        .await?;
    let n = sqlx::query("SELECT * FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node)
        .fetch_one(&mut *tx)
        .await?;
    if n.get::<Uuid, _>("pool_id") != pool {
        return Err(ApiError::Conflict(
            "node changed pools; retry the request".into(),
        ));
    }
    if n.get::<bool, _>("revoked") {
        return Err(ApiError::Conflict("node is revoked".into()));
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_updates WHERE pool_id=$1 AND finished_at IS NULL",
    )
    .bind(pool)
    .fetch_one(&mut *tx)
    .await?;
    let limit: i32 =
        sqlx::query_scalar("SELECT agent_update_concurrency FROM node_pools WHERE id=$1")
            .bind(pool)
            .fetch_one(&mut *tx)
            .await?;
    if count >= i64::from(limit) {
        return Err(ApiError::Conflict("pool upgrade capacity is in use".into()));
    }
    let new_id = Uuid::new_v4();
    sqlx::query("INSERT INTO agent_updates(id,node_id,pool_id,release_id,previous_version,retry_of) VALUES($1,$2,$3,$4,$5,$6)").bind(new_id).bind(node).bind(n.get::<Uuid,_>("pool_id")).bind(u.get::<Uuid,_>("release_id")).bind(n.get::<Option<String>,_>("agent_current_version")).bind(id).execute(&mut *tx).await?;
    sqlx::query("UPDATE nodes SET ota_draining=true WHERE id=$1")
        .bind(node)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE node_pools SET agent_updates_paused=false WHERE id=$1")
        .bind(pool)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO audit_events(actor,action,resource_id,details) VALUES($1,'agent_update_retried',$2,$3)").bind(auth.user_id.to_string()).bind(new_id.to_string()).bind(json!({"retry_of":id})).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"id":new_id})))
}
pub async fn cancel(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    let mut tx = state.pool.begin().await?;
    let node: Uuid = sqlx::query_scalar("SELECT node_id FROM agent_updates WHERE id=$1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node)
        .execute(&mut *tx)
        .await?;
    let row=sqlx::query("UPDATE agent_updates SET cancel_requested=true,last_command_at=NULL,updated_at=now() WHERE id=$1 AND finished_at IS NULL AND status IN ('queued','offered','downloading','draining') RETURNING status").bind(id).fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        return Err(ApiError::Conflict(
            "update is activating or already finished".into(),
        ));
    };
    if row.get::<String, _>("status") == "queued" {
        sqlx::query("UPDATE agent_updates SET status='canceled',finished_at=now() WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE nodes SET ota_draining=false WHERE id=$1")
            .bind(node)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("INSERT INTO audit_events(actor,action,resource_id,details) VALUES($1,'agent_update_cancel_requested',$2,'{}')").bind(auth.user_id.to_string()).bind(id.to_string()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"ok":true})))
}
