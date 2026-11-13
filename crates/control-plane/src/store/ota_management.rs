//! Atomic Emissary update policy, retry and cancellation operations.
use super::ControlStore;
use super::dto;
use crate::error::{ApiError, Result};
use artifactory_database::{params, sql};
use serde::Deserialize;
use serde_json::json;
use toasty::stmt::Type;

use uuid::Uuid;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub(crate) release_id: Option<Uuid>,
    pub(crate) concurrency: Option<i32>,
    pub(crate) paused: Option<bool>,
}
impl ControlStore {
    pub async fn set_node_target(&self, id: Uuid, input: &Target, actor: Uuid) -> Result<()> {
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        let revoked: bool = sql::scalar(
            &mut tx,
            "SELECT revoked FROM nodes WHERE id=$1 FOR UPDATE",
            params![id],
        )
        .await?;
        if revoked {
            return Err(ApiError::Conflict("revoked nodes cannot upgrade".into()));
        }
        let busy: bool = sql::scalar(&mut tx, "SELECT EXISTS(SELECT 1 FROM emissary_updates WHERE node_id=$1 AND finished_at IS NULL)", params![id])
    .await?;
        if busy {
            return Err(ApiError::Conflict(
                "an update is already active; cancel or wait for it first".into(),
            ));
        }
        sql::execute(
            &mut tx,
            "UPDATE nodes SET emissary_target_release=$2 WHERE id=$1",
            params![id, input.release_id],
        )
        .await?;
        super::models::AuditEvent::create()
            .actor(actor.to_string())
            .action("emissary_node_target_changed")
            .resource_id(id.to_string())
            .details(toasty::Json(
                (json!({"release_id":input.release_id})).clone(),
            ))
            .exec(&mut tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_pool_target(&self, id: Uuid, input: &Target, actor: Uuid) -> Result<()> {
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        let changed=sql::execute(&mut tx, "UPDATE node_pools SET emissary_target_release=$2,emissary_update_concurrency=coalesce($3,emissary_update_concurrency),emissary_updates_paused=coalesce($4,emissary_updates_paused) WHERE id=$1", params![id, input.release_id, input.concurrency, input.paused]).await?;
        if changed == 0 {
            return Err(ApiError::NotFound);
        }
        super::models::AuditEvent::create().actor(actor.to_string()).action("emissary_pool_policy_changed").resource_id(id.to_string()).details(toasty::Json((json!({"release_id":input.release_id,"concurrency":input.concurrency,"paused":input.paused})).clone())).exec(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn retry_update(&self, id: Uuid, actor: Uuid) -> Result<Uuid> {
        let u = sql::record(
            &mut self.pool.clone(),
            "SELECT * FROM emissary_updates WHERE id=$1",
            params![id],
            &[
                ("id", Type::Uuid),
                ("generation", Type::I64),
                ("node_id", Type::Uuid),
                ("pool_id", Type::Uuid),
                ("release_id", Type::Uuid),
                ("previous_version", Type::String),
                ("status", Type::String),
                ("cancel_requested", Type::Bool),
                ("error", Type::String),
                ("retry_of", Type::Uuid),
                ("created_at", Type::Timestamp),
                ("updated_at", Type::Timestamp),
                ("finished_at", Type::Timestamp),
                ("last_command_at", Type::Timestamp),
            ],
        )
        .await?;
        if !["failed", "rolled_back", "canceled"].contains(&u.decode::<String>("status")?.as_str())
        {
            return Err(ApiError::Conflict(
                "only a failed, rolled back or canceled update can be retried".into(),
            ));
        }
        let node: Uuid = u.decode("node_id")?;
        let pool: Uuid = sql::scalar(
            &mut self.pool.clone(),
            "SELECT pool_id FROM nodes WHERE id=$1",
            params![node],
        )
        .await?;
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        sql::execute(
            &mut tx,
            "SELECT pg_advisory_xact_lock(hashtextextended($1,57))",
            params![pool.to_string()],
        )
        .await?;
        let n = sql::record(
            &mut tx,
            "SELECT pool_id,revoked,emissary_current_version FROM nodes WHERE id=$1 FOR UPDATE",
            params![node],
            &[
                ("pool_id", Type::Uuid),
                ("revoked", Type::Bool),
                ("emissary_current_version", Type::String),
            ],
        )
        .await?;
        if n.decode::<Uuid>("pool_id")? != pool {
            return Err(ApiError::Conflict(
                "node changed pools; retry the request".into(),
            ));
        }
        if n.decode::<bool>("revoked")? {
            return Err(ApiError::Conflict("node is revoked".into()));
        }
        let count: i64 = sql::scalar(
            &mut tx,
            "SELECT count(*) FROM emissary_updates WHERE pool_id=$1 AND finished_at IS NULL",
            params![pool],
        )
        .await?;
        let limit: i32 = sql::scalar(
            &mut tx,
            "SELECT emissary_update_concurrency FROM node_pools WHERE id=$1",
            params![pool],
        )
        .await?;
        if count >= i64::from(limit) {
            return Err(ApiError::Conflict("pool upgrade capacity is in use".into()));
        }
        let new_id = Uuid::new_v4();
        super::models::EmissaryUpdate::create()
            .id(new_id)
            .node_id(node)
            .pool_id(n.decode::<Uuid>("pool_id")?)
            .release_id(u.decode::<Uuid>("release_id")?)
            .previous_version(n.decode::<Option<String>>("emissary_current_version")?)
            .retry_of(id)
            .exec(&mut tx)
            .await?;
        sql::execute(
            &mut tx,
            "UPDATE nodes SET ota_draining=true WHERE id=$1",
            params![node],
        )
        .await?;
        sql::execute(
            &mut tx,
            "UPDATE node_pools SET emissary_updates_paused=false WHERE id=$1",
            params![pool],
        )
        .await?;
        super::models::AuditEvent::create()
            .actor(actor.to_string())
            .action("emissary_update_retried")
            .resource_id(new_id.to_string())
            .details(toasty::Json((json!({"retry_of":id})).clone()))
            .exec(&mut tx)
            .await?;
        tx.commit().await?;
        Ok(new_id)
    }

    pub async fn cancel_update(&self, id: Uuid, actor: Uuid) -> Result<()> {
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        let node: Uuid = sql::scalar(
            &mut tx,
            "SELECT node_id FROM emissary_updates WHERE id=$1",
            params![id],
        )
        .await?;
        sql::execute(
            &mut tx,
            "SELECT id FROM nodes WHERE id=$1 FOR UPDATE",
            params![node],
        )
        .await?;
        let row=sql::optional_record(&mut tx, "UPDATE emissary_updates SET cancel_requested=true,last_command_at=NULL,updated_at=now() WHERE id=$1 AND finished_at IS NULL AND status IN ('queued','offered','downloading','draining') RETURNING status", params![id], &[("status", Type::String)]).await?;
        let Some(row) = row else {
            return Err(ApiError::Conflict(
                "update is activating or already finished".into(),
            ));
        };
        if row.decode::<String>("status")? == "queued" {
            sql::execute(
                &mut tx,
                "UPDATE emissary_updates SET status='canceled',finished_at=now() WHERE id=$1",
                params![id],
            )
            .await?;
            sql::execute(
                &mut tx,
                "UPDATE nodes SET ota_draining=false WHERE id=$1",
                params![node],
            )
            .await?;
        }
        super::models::AuditEvent::create()
            .actor(actor.to_string())
            .action("emissary_update_cancel_requested")
            .resource_id(id.to_string())
            .details(toasty::Json(serde_json::json!({})))
            .exec(&mut tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn emissary_releases(&self) -> Result<Vec<dto::EmissaryRelease>> {
        Ok(sql::scalars(
            &mut self.pool.clone(),
            "SELECT to_jsonb(r) FROM emissary_releases r ORDER BY created_at DESC",
            params![],
        )
        .await?)
    }

    pub async fn emissary_updates(&self) -> Result<Vec<dto::EmissaryUpdate>> {
        Ok(sql::scalars(&mut self.pool.clone(), "SELECT to_jsonb(u)||jsonb_build_object('version',r.version,'node_name',n.name) FROM emissary_updates u JOIN emissary_releases r ON r.id=u.release_id JOIN nodes n ON n.id=u.node_id ORDER BY u.created_at DESC LIMIT 500", params![]).await?)
    }
}
