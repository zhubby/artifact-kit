//! Durable work and connection ownership shared by Control replicas.
use super::ControlStore;
use crate::{crypto, recipe::Target};
use anyhow::Result;
use artifactory_database::{params, sql};
use serde_json::{Value, json};
use toasty::stmt::Type;
use uuid::Uuid;

pub(crate) struct DeviceCommand {
    pub id: Uuid,
    pub node: Uuid,
    pub session: Uuid,
    pub attempt: Uuid,
    pub kind: String,
    pub lease_token: String,
}

impl ControlStore {
    pub(crate) async fn listen_work(url: &str) -> Result<artifactory_database::Listener> {
        Ok(artifactory_database::listen(url, "artifactory_work").await?)
    }

    pub(crate) async fn ready(&self) -> Result<()> {
        sql::execute(&mut self.pool.clone(), "SELECT 1", params![]).await?;
        Ok(())
    }

    pub(crate) async fn clear_connection_owner(
        &self,
        node: Uuid,
        session: Uuid,
        owner: Uuid,
    ) -> Result<()> {
        sql::execute(&mut self.pool.clone(), "UPDATE nodes SET connection_owner=NULL WHERE id=$1 AND active_session=$2 AND connection_owner=$3", params![node, session, owner]).await?;
        Ok(())
    }

    pub(crate) async fn drain_owner(&self, owner: Uuid) -> Result<()> {
        loop {
            let cleared = sql::execute(&mut self.pool.clone(), "WITH batch AS (SELECT id FROM nodes WHERE connection_owner=$1 ORDER BY id LIMIT 64 FOR UPDATE SKIP LOCKED) UPDATE nodes n SET connection_owner=NULL FROM batch b WHERE n.id=b.id AND n.connection_owner=$1", params![owner]).await?;
            if cleared == 0 {
                let remaining: bool = sql::scalar(
                    &mut self.pool.clone(),
                    "SELECT EXISTS(SELECT 1 FROM nodes WHERE connection_owner=$1)",
                    params![owner],
                )
                .await?;
                if !remaining {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
    }

    pub(crate) async fn maintain<F, Fut>(&self, action: F) -> Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let mut db = self.pool.clone();
        let mut guard = db.transaction().await?;
        let acquired: bool = sql::scalar(
            &mut guard,
            "SELECT pg_try_advisory_xact_lock(hashtextextended('artifactory:maintenance',12))",
            params![],
        )
        .await?;
        if acquired {
            action().await?;
        }
        guard.commit().await?;
        Ok(())
    }

    pub(crate) async fn retain_commands(&self) -> Result<()> {
        sql::execute(&mut self.pool.clone(), "DELETE FROM device_commands WHERE id IN (SELECT id FROM device_commands WHERE acknowledged_at<now()-interval '7 days' ORDER BY acknowledged_at LIMIT 1000)", params![]).await?;
        Ok(())
    }

    pub(crate) async fn pending_commands(
        &self,
        owner: Uuid,
        batch: i64,
        kind: &str,
    ) -> Result<Vec<DeviceCommand>> {
        let rows = sql::records(&mut self.pool.clone(), "WITH pending AS (SELECT c.id,n.active_session,a.lease_token FROM device_commands c JOIN nodes n ON n.id=c.node_id JOIN attempts a ON a.id=c.attempt_id JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE c.kind=$3 AND c.acknowledged_at IS NULL AND c.next_delivery_at<=clock_timestamp() AND n.connection_owner=$1 AND NOT n.revoked AND n.certificate_expires_at>clock_timestamp() AND a.lease_expires_at>clock_timestamp() AND ((c.kind='assign' AND a.status='leased' AND NOT b.cancel_requested) OR (c.kind='cancel' AND a.status IN ('leased','running') AND b.cancel_requested)) ORDER BY c.next_delivery_at,c.id LIMIT $2 FOR UPDATE OF c SKIP LOCKED), claimed AS (UPDATE device_commands c SET next_delivery_at=clock_timestamp()+interval '2 seconds' FROM pending p WHERE c.id=p.id RETURNING c.*) SELECT c.id,c.node_id,c.attempt_id,c.kind,p.active_session,p.lease_token FROM claimed c JOIN pending p ON p.id=c.id", params![owner,batch,kind], &[("id",Type::Uuid),("node_id",Type::Uuid),("attempt_id",Type::Uuid),("kind",Type::String),("active_session",Type::Uuid),("lease_token",Type::String)]).await?;
        rows.into_iter()
            .map(|row| {
                Ok(DeviceCommand {
                    id: row.decode("id")?,
                    node: row.decode("node_id")?,
                    session: row.decode("active_session")?,
                    attempt: row.decode("attempt_id")?,
                    kind: row.decode("kind")?,
                    lease_token: row.decode("lease_token")?,
                })
            })
            .collect()
    }

    pub(crate) async fn assignment_job(&self, attempt: Uuid) -> Result<Uuid> {
        Ok(sql::scalar(
            &mut self.pool.clone(),
            "SELECT job_id FROM attempts WHERE id=$1",
            params![attempt],
        )
        .await?)
    }

    pub(crate) async fn command_authorized(
        &self,
        command: &DeviceCommand,
        owner: Uuid,
    ) -> Result<bool> {
        Ok(sql::scalar(&mut self.pool.clone(), "SELECT EXISTS(SELECT 1 FROM nodes n JOIN attempts a ON a.node_id=n.id JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE n.id=$1 AND n.active_session=$2 AND n.connection_owner=$3 AND NOT n.revoked AND n.certificate_expires_at>clock_timestamp() AND a.id=$4 AND a.lease_expires_at>clock_timestamp() AND (($5='assign' AND a.status='leased' AND NOT b.cancel_requested) OR ($5='cancel' AND a.status IN ('leased','running') AND b.cancel_requested)))", params![command.node,command.session,owner,command.attempt,command.kind]).await?)
    }

    pub(crate) async fn ota_candidates(
        &self,
        owner: Uuid,
        batch: i64,
    ) -> Result<Vec<(Uuid, Uuid)>> {
        Ok(sql::tuples(&mut self.pool.clone(), "SELECT n.id,n.active_session FROM nodes n JOIN node_pools p ON p.id=n.pool_id WHERE n.connection_owner=$1 AND NOT n.revoked AND n.certificate_expires_at>clock_timestamp() AND n.last_seen>now()-interval '30 seconds' AND (EXISTS(SELECT 1 FROM emissary_updates u WHERE u.node_id=n.id AND u.finished_at IS NULL AND (u.last_command_at IS NULL OR u.last_command_at<now()-interval '10 seconds')) OR (coalesce(n.emissary_target_release,p.emissary_target_release) IS NOT NULL AND NOT p.emissary_updates_paused AND EXISTS(SELECT 1 FROM emissary_releases r WHERE r.id=coalesce(n.emissary_target_release,p.emissary_target_release) AND r.version IS DISTINCT FROM n.emissary_current_version))) ORDER BY n.last_seen DESC LIMIT $2", params![owner,batch]).await?)
    }
    pub(crate) async fn schedule_one(&self) -> Result<bool> {
        let mut db = self.pool.clone();
        let mut tx = db.transaction().await?;
        let candidate = sql::optional_record(&mut tx, "SELECT j.id,j.target_config,j.plan_revision_id,b.source_snapshot_id,b.goal,b.planning_context,p.pool_ids,p.signing_profiles AS permitted_signing FROM jobs j JOIN builds b ON b.id=j.build_id JOIN projects p ON p.id=b.project_id WHERE j.status='queued' AND j.dispatch_after<=clock_timestamp() AND b.status IN ('queued','running') AND NOT b.cancel_requested ORDER BY j.dispatch_after,j.created_at,j.id LIMIT 1 FOR UPDATE OF b,j SKIP LOCKED", params![], &[("id", Type::Uuid), ("target_config", Type::String), ("plan_revision_id", Type::Uuid), ("source_snapshot_id", Type::Uuid), ("goal", Type::String), ("planning_context", Type::String), ("pool_ids", Type::List(Box::new(Type::Uuid))), ("permitted_signing", Type::List(Box::new(Type::String)))]).await?;
        let Some(candidate) = candidate else {
            return Ok(false);
        };
        let job: Uuid = candidate.decode("id")?;
        let target: Target = serde_json::from_value(candidate.decode("target_config")?)?;
        let permitted: Vec<String> = candidate.decode("permitted_signing")?;
        let mut pools: Vec<Uuid> = candidate.decode("pool_ids")?;
        let mut capabilities = target.capabilities.clone();
        capabilities.push("git".into());
        capabilities.extend(target.steps.iter().map(|s| {
            s.shell.clone().unwrap_or_else(|| {
                if target.os == "windows" {
                    "powershell".into()
                } else {
                    "bash".into()
                }
            })
        }));
        if candidate.decode::<Option<String>>("goal")?.is_some() {
            capabilities.extend(["source-cache-v1".into(), "external-build-plan-v1".into()]);
            let context: Value = candidate.decode("planning_context")?;
            let allowed: Vec<Uuid> =
                serde_json::from_value(context.get("pool_ids").cloned().unwrap_or(json!([])))?;
            pools.retain(|pool| allowed.contains(pool));
        }
        let node = if target
            .signing_profiles
            .iter()
            .all(|profile| permitted.contains(profile))
        {
            sql::optional_record(&mut tx, "SELECT n.id FROM nodes n WHERE n.pool_id=ANY($1) AND n.os=$2 AND n.arch=$3 AND n.labels @> $4 AND n.capabilities @> $5 AND $6::text[] <@ n.signing_profiles AND n.active_session IS NOT NULL AND n.connection_owner IS NOT NULL AND NOT n.revoked AND NOT n.draining AND NOT n.ota_draining AND n.certificate_expires_at>clock_timestamp() AND n.last_seen>clock_timestamp()-interval '30 seconds' AND n.slots>(SELECT count(*) FROM attempts a WHERE a.node_id=n.id AND a.status IN ('leased','running') AND a.lease_expires_at>clock_timestamp()) ORDER BY n.id LIMIT 1 FOR UPDATE OF n SKIP LOCKED", params![&pools, &target.os, &target.arch, serde_json::to_value(&target.labels)?, json!(capabilities), &target.signing_profiles], &[("id", Type::Uuid)]).await?
        } else {
            None
        };
        let node = if let Some(node) = node {
            // Use a new READ COMMITTED snapshot after acquiring the node lock.
            // Another assignment may have committed after the selection began.
            let valid: bool = sql::scalar(&mut tx, "SELECT active_session IS NOT NULL AND connection_owner IS NOT NULL AND NOT revoked AND NOT draining AND NOT ota_draining AND certificate_expires_at>clock_timestamp() AND last_seen>clock_timestamp()-interval '30 seconds' AND slots>(SELECT count(*) FROM attempts WHERE node_id=$1 AND status IN ('leased','running') AND lease_expires_at>clock_timestamp()) FROM nodes WHERE id=$1", params![node.decode::<Uuid>("id")?]).await?;
            valid.then_some(node)
        } else {
            None
        };
        if let Some(node) = node {
            // The node row serializes slots and OTA admission across replicas.
            sql::execute(&mut tx, "INSERT INTO attempts(id,job_id,node_id,lease_token,lease_expires_at,status,command_id,plan_revision_id,source_snapshot_id) VALUES($1,$2,$3,$4,clock_timestamp()+interval '90 seconds','leased',$5,$6,$7)", params![Uuid::new_v4(), job, node.decode::<Uuid>("id")?, crypto::token(), Uuid::new_v4(), candidate.decode::<Option<Uuid>>("plan_revision_id")?, candidate.decode::<Option<Uuid>>("source_snapshot_id")?]).await?;
            sql::execute(
                &mut tx,
                "UPDATE jobs SET status='leased' WHERE id=$1",
                params![job],
            )
            .await?;
        } else {
            // Rotate incompatible/saturated work so the oldest platform cannot
            // starve later jobs. Periodic reconciliation bounds lost wakeups.
            sql::execute(
                &mut tx,
                "UPDATE jobs SET dispatch_after=clock_timestamp()+interval '5 seconds' WHERE id=$1",
                params![job],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }
}
