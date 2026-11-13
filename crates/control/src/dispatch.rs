//! Durable, replayable device commands delivered only by the current connection owner.
use crate::{protocol::Command, scheduler, state::AppState};
use anyhow::Result;
use futures::{StreamExt, stream};
use sqlx::Row;
use uuid::Uuid;

/// Deliver a bounded batch; a successful socket write is never treated as execution ACK.
///
/// # Errors
/// Returns database claim errors. Individual delivery failures remain pending for replay.
pub async fn deliver(state: &AppState) -> Result<()> {
    deliver_cancellations(state).await?;
    deliver_assignments(state).await
}

pub(crate) async fn deliver_cancellations(state: &AppState) -> Result<()> {
    deliver_kind(state, "cancel").await
}

pub(crate) async fn deliver_assignments(state: &AppState) -> Result<()> {
    deliver_kind(state, "assign").await
}

async fn deliver_kind(state: &AppState, kind: &str) -> Result<()> {
    if state.coordination.is_draining() {
        return Ok(());
    }
    let rows = sqlx::query(
        "WITH pending AS (SELECT c.id,n.active_session,a.lease_token FROM device_commands c
         JOIN nodes n ON n.id=c.node_id JOIN attempts a ON a.id=c.attempt_id
         JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id
         WHERE c.kind=$3 AND c.acknowledged_at IS NULL AND c.next_delivery_at<=clock_timestamp()
           AND n.connection_owner=$1 AND NOT n.revoked AND n.certificate_expires_at>clock_timestamp()
           AND a.lease_expires_at>clock_timestamp()
           AND ((c.kind='assign' AND a.status='leased' AND NOT b.cancel_requested)
             OR (c.kind='cancel' AND a.status IN ('leased','running') AND b.cancel_requested))
         ORDER BY c.next_delivery_at,c.id LIMIT $2 FOR UPDATE OF c SKIP LOCKED),
         claimed AS (UPDATE device_commands c SET next_delivery_at=clock_timestamp()+interval '2 seconds'
         FROM pending p WHERE c.id=p.id RETURNING c.*)
         SELECT c.*,p.active_session,p.lease_token FROM claimed c JOIN pending p ON p.id=c.id",
    )
    .bind(state.coordination.id)
    .bind(state.settings.control.scheduler_batch)
    .bind(kind)
    .fetch_all(&state.pool)
    .await?;
    // Descriptor preparation may contact Source. Bound it independently of the
    // command channel so one unavailable source cannot block unrelated devices.
    stream::iter(rows)
        .for_each_concurrent(8, |row| async move {
            let node: Uuid = row.get("node_id");
            let session: Uuid = row.get("active_session");
            let connection = state.connections.read().await.get(&node).cloned();
            let Some(connection) = connection.filter(|c| c.session == session) else {
                return;
            };
            let attempt: Uuid = row.get("attempt_id");
            let kind: String = row.get("kind");
            let assignment = if kind == "assign" {
                let result = async {
                    let job = sqlx::query_scalar("SELECT job_id FROM attempts WHERE id=$1")
                        .bind(attempt)
                        .fetch_one(&state.pool)
                        .await?;
                    scheduler::assignment(state, job, attempt).await
                }
                .await;
                match result {
                    Ok(assignment) => Some(assignment),
                    Err(error) => {
                        tracing::warn!(%attempt,%error,"device assignment delivery deferred");
                        return;
                    }
                }
            } else {
                None
            };
            if state.coordination.is_draining() {
                return;
            }
            // Source preparation can outlive a reconnect, cancellation or node
            // revocation. Recheck immediately before exposing the assignment.
            let valid: Result<bool,sqlx::Error>=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes n JOIN attempts a ON a.node_id=n.id JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE n.id=$1 AND n.active_session=$2 AND n.connection_owner=$3 AND NOT n.revoked AND n.certificate_expires_at>clock_timestamp() AND a.id=$4 AND a.lease_expires_at>clock_timestamp() AND (($5='assign' AND a.status='leased' AND NOT b.cancel_requested) OR ($5='cancel' AND a.status IN ('leased','running') AND b.cancel_requested)))")
                .bind(node).bind(session).bind(state.coordination.id).bind(attempt).bind(&kind).fetch_one(&state.pool).await;
            if !matches!(valid,Ok(true)) {return;}
            let command = Command {
                command_id: row.get::<Uuid, _>("id").to_string(),
                kind,
                attempt_id: attempt.to_string(),
                lease_token: row.get("lease_token"),
                lease_seconds: 90,
                assignment,
                ..Default::default()
            };
            // Full/closed queues retain the durable command for a later replay.
            let _ = connection.sender.try_send(Ok(command));
        })
        .await;
    Ok(())
}

pub(crate) async fn deliver_ota(state: &AppState) -> Result<()> {
    let rows = sqlx::query("SELECT n.id,n.active_session FROM nodes n JOIN node_pools p ON p.id=n.pool_id
        WHERE n.connection_owner=$1 AND NOT n.revoked AND n.certificate_expires_at>clock_timestamp()
          AND n.last_seen>now()-interval '30 seconds'
          AND (EXISTS(SELECT 1 FROM emissary_updates u WHERE u.node_id=n.id AND u.finished_at IS NULL
               AND (u.last_command_at IS NULL OR u.last_command_at<now()-interval '10 seconds'))
            OR (coalesce(n.emissary_target_release,p.emissary_target_release) IS NOT NULL
                AND NOT p.emissary_updates_paused AND EXISTS(SELECT 1 FROM emissary_releases r
                WHERE r.id=coalesce(n.emissary_target_release,p.emissary_target_release)
                AND r.version IS DISTINCT FROM n.emissary_current_version)))
        ORDER BY n.last_seen DESC LIMIT $2")
        .bind(state.coordination.id).bind(state.settings.control.scheduler_batch)
        .fetch_all(&state.pool).await?;
    for row in rows {
        let node = row.get("id");
        let session = row.get("active_session");
        let connection = state.connections.read().await.get(&node).cloned();
        if let Some(connection) = connection.filter(|c| c.session == session)
            && let Some(command) = crate::ota::next(state, node, session).await?
        {
            let _ = connection.sender.try_send(Ok(command));
        }
    }
    Ok(())
}
