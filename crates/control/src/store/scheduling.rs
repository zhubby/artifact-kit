//! Scheduler persistence. Claim, publish and resolver lock lifetimes are owned here.
use super::ControlStore;
use crate::{
    crypto,
    recipe::{Recipe, Target},
};
use anyhow::{Context, Result, ensure};
use artifactory_database::{params, sql};
use serde_json::{Value, json};
use toasty::stmt::Type;

use uuid::Uuid;
pub(crate) struct CanceledAttempt {
    pub id: Uuid,
    pub lease_token: String,
}
pub(crate) struct LeasedJob {
    pub job: Uuid,
    pub attempt: Uuid,
    pub command_id: Uuid,
    pub lease_token: String,
}
pub(crate) struct AssignmentContext {
    pub target: String,
    pub target_config: Value,
    pub plan_revision_id: Option<Uuid>,
    pub source_snapshot_id: Option<Uuid>,
    pub id: Uuid,
    pub commit_sha: String,
    pub config_snapshot: Value,
    pub parameters: Value,
    pub repository_url: String,
    pub credential_id: Option<Uuid>,
    pub plan: Option<Value>,
}
pub(crate) struct ResolutionInput {
    pub project_id: Uuid,
    pub requested_ref: String,
    pub parameters: Value,
    pub selected_targets: Vec<String>,
    pub release_version: Option<String>,
    pub goal: Option<String>,
    pub planning_context: Value,
    pub model_config_snapshot: Option<Value>,
    pub repository_url: String,
    pub credential_id: Option<Uuid>,
    pub signing_profiles: Vec<String>,
}
pub(crate) enum ResolvedSource {
    Planned {
        commit: String,
        snapshot_id: Uuid,
        model: Value,
    },
    Recipe {
        commit: String,
        yaml: String,
        recipe: Recipe,
        targets: Vec<(String, Target)>,
    },
}

impl ControlStore {
    pub async fn expire(&self) -> Result<()> {
        let mut transaction_db = self.pool.clone();
        let mut transaction = transaction_db.transaction().await?;
        let rows=sql::records(&mut transaction, "SELECT a.id,a.job_id,a.status,b.cancel_requested FROM attempts a JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE a.status IN ('leased','running') AND a.lease_expires_at<=now() FOR UPDATE OF a,j SKIP LOCKED", params![], &[("id", Type::Uuid),("job_id", Type::Uuid),("status", Type::String),("cancel_requested", Type::Bool)]).await?;
        for row in rows {
            let started = row.decode::<String>("status")? == "running";
            let canceled: bool = row.decode("cancel_requested")?;
            sql::execute(&mut transaction, "UPDATE attempts SET status=$2,error='execution lease expired',finished_at=now() WHERE id=$1", params![row.decode::<Uuid>("id")?, if canceled {"canceled"} else {"lost"}]).await?;
            sql::execute(
                &mut transaction,
                "UPDATE jobs SET status=$2 WHERE id=$1",
                params![
                    row.decode::<Uuid>("job_id")?,
                    if canceled {
                        "canceled"
                    } else if started {
                        "lost"
                    } else {
                        "queued"
                    }
                ],
            )
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn refresh_builds(&self) -> Result<()> {
        sql::execute(&mut self.pool.clone(), "WITH summaries AS (SELECT b.id, CASE WHEN b.cancel_requested THEN CASE WHEN bool_or(j.status IN ('leased','running')) THEN 'canceling' ELSE 'canceled' END WHEN bool_or(j.status='running') THEN 'running' WHEN bool_or(j.status IN ('queued','leased')) THEN 'queued' WHEN bool_or(j.status='failed') THEN 'failed' WHEN bool_or(j.status='lost') THEN 'lost' WHEN bool_or(j.status='canceled') THEN 'canceled' ELSE 'succeeded' END AS status FROM builds b JOIN jobs j ON j.build_id=b.id WHERE b.status NOT IN ('resolving','planning','replanning','needs_input') GROUP BY b.id) UPDATE builds b SET status=s.status,finished_at=CASE WHEN s.status IN ('succeeded','failed','lost','canceled') THEN coalesce(b.finished_at,now()) ELSE NULL END FROM summaries s WHERE b.id=s.id", params![]).await?;
        Ok(())
    }

    pub async fn queue_revisions(&self) -> Result<()> {
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        let rows=sql::records(&mut tx, "SELECT id,model_config_snapshot FROM builds b WHERE goal IS NOT NULL AND current_plan_id IS NOT NULL AND status='failed' AND NOT cancel_requested AND revision_count<2 AND (SELECT state FROM planning_runs WHERE build_id=b.id ORDER BY created_at DESC LIMIT 1)='succeeded' AND EXISTS(SELECT 1 FROM jobs j JOIN LATERAL (SELECT failure_kind FROM attempts WHERE job_id=j.id ORDER BY created_at DESC LIMIT 1) a ON true WHERE j.build_id=b.id AND j.status='failed' AND a.failure_kind='execution') ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT 10", params![], &[("id", Type::Uuid),("model_config_snapshot", Type::String)]).await?;
        for row in rows {
            let id: Uuid = row.decode("id")?;
            let failed:Vec<String>=sql::scalars(&mut tx, "SELECT j.target FROM jobs j JOIN LATERAL (SELECT failure_kind FROM attempts WHERE job_id=j.id ORDER BY created_at DESC LIMIT 1) a ON true WHERE j.build_id=$1 AND j.status='failed' AND a.failure_kind='execution'", params![id]).await?;
            sql::execute(&mut tx, "UPDATE builds SET revision_count=revision_count+1,status='replanning',finished_at=NULL WHERE id=$1", params![id]).await?;
            super::models::PlanningRun::create()
                .id(Uuid::new_v4())
                .build_id(id)
                .purpose("revision")
                .model_config(toasty::Json(
                    (row.decode::<Value>("model_config_snapshot")?).clone(),
                ))
                .failed_targets(failed)
                .exec(&mut tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn publish_transaction(&self, id: Uuid) -> Result<Uuid> {
        let mut transaction_db = self.pool.clone();
        let mut transaction = transaction_db.transaction().await?;
        let build = sql::record(
            &mut transaction,
            "SELECT * FROM builds WHERE id=$1 FOR UPDATE",
            params![id],
            &[
                ("id", Type::Uuid),
                ("project_id", Type::Uuid),
                ("requested_ref", Type::String),
                ("commit_sha", Type::String),
                ("config_yaml", Type::String),
                ("config_snapshot", Type::String),
                ("parameters", Type::String),
                ("selected_targets", Type::List(Box::new(Type::String))),
                ("status", Type::String),
                ("error", Type::String),
                ("release_version", Type::String),
                ("publish_status", Type::String),
                ("publish_error", Type::String),
                ("cancel_requested", Type::Bool),
                ("requested_by", Type::Uuid),
                ("created_at", Type::Timestamp),
                ("finished_at", Type::Timestamp),
                ("source_repository_url", Type::String),
                ("source_credential_id", Type::Uuid),
                ("goal", Type::String),
                ("target_requirements", Type::String),
                ("planning_context", Type::String),
                ("model_config_snapshot", Type::String),
                ("source_snapshot_id", Type::Uuid),
                ("revision_count", Type::I32),
                ("clarification", Type::String),
                ("current_plan_id", Type::Uuid),
                ("planner_backend", Type::String),
            ],
        )
        .await?;
        ensure!(
            build.decode::<String>("status")? == "succeeded"
                && !build.decode::<bool>("cancel_requested")?,
            "only a successful build can be published"
        );
        let version: Option<String> = build.decode("release_version")?;
        let version = version.ok_or_else(|| anyhow::anyhow!("release version is required"))?;
        let recipe: Recipe = serde_json::from_value(build.decode("config_snapshot")?)?;
        ensure!(
            recipe.release.enabled,
            "publishing is disabled by repository configuration"
        );
        let key = format!("build:{id}");
        let project: Uuid = build.decode("project_id")?;
        if let Some(existing) = sql::optional_scalar::<Uuid>(
            &mut transaction,
            "SELECT id FROM releases WHERE project_id=$1 AND idempotency_key=$2",
            params![project, &key],
        )
        .await?
        {
            return Ok(existing);
        }
        let artifacts:Vec<Uuid>=sql::scalars(&mut transaction, "SELECT ar.id FROM artifacts ar JOIN attempts a ON a.id=ar.attempt_id JOIN jobs j ON j.id=a.job_id WHERE j.build_id=$1 AND j.status='succeeded' AND a.status='succeeded' AND a.plan_revision_id IS NOT DISTINCT FROM j.plan_revision_id AND a.source_snapshot_id IS NOT DISTINCT FROM (SELECT source_snapshot_id FROM builds WHERE id=$1) AND a.id=(SELECT id FROM attempts WHERE job_id=j.id ORDER BY created_at DESC,id DESC LIMIT 1)", params![id]).await?;
        let jobs: i64 = sql::scalar(
            &mut transaction,
            "SELECT count(*) FROM jobs WHERE build_id=$1",
            params![id],
        )
        .await?;
        let complete:i64=sql::scalar(&mut transaction, "SELECT count(DISTINCT j.id) FROM jobs j JOIN attempts a ON a.job_id=j.id AND a.status='succeeded' JOIN artifacts ar ON ar.attempt_id=a.id WHERE j.build_id=$1 AND j.status='succeeded' AND ar.id=ANY($2)", params![id, &artifacts]).await?;
        ensure!(
            jobs > 0 && jobs == complete,
            "every target must have verified artifacts"
        );
        let release = Uuid::new_v4();
        super::models::Release::create()
            .id(release)
            .project_id(project)
            .build_id(id)
            .version(version)
            .idempotency_key(key)
            .commit_sha(build.decode::<String>("commit_sha")?)
            .exec(&mut transaction)
            .await
            .context("release version already exists or publication failed")?;
        for artifact in artifacts {
            super::models::ReleaseArtifact::create()
                .release_id(release)
                .artifact_id(artifact)
                .exec(&mut transaction)
                .await?;
        }
        sql::execute(
            &mut transaction,
            "UPDATE builds SET publish_status='succeeded',publish_error=NULL WHERE id=$1",
            params![id],
        )
        .await?;
        super::models::PublishAttempt::create()
            .id(Uuid::new_v4())
            .build_id(id)
            .status("succeeded")
            .exec(&mut transaction)
            .await?;
        transaction.commit().await?;
        Ok(release)
    }

    pub(crate) async fn publication_failed(&self, id: Uuid, error: &str) -> Result<()> {
        sql::execute(&mut self.pool.clone(), "UPDATE builds SET publish_status='failed',publish_error=$2 WHERE id=$1 AND publish_status<>'succeeded'", params![id, error]).await?;
        super::models::PublishAttempt::create()
            .id(Uuid::new_v4())
            .build_id(id)
            .status("failed")
            .error(error)
            .exec(&mut self.pool.clone())
            .await?;
        Ok(())
    }

    pub(crate) async fn canceled_attempts(&self, node_id: Uuid) -> Result<Vec<CanceledAttempt>> {
        let rows=sql::records(&mut self.pool.clone(), "SELECT a.id,a.lease_token FROM attempts a JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE a.node_id=$1 AND a.status IN ('leased','running') AND b.cancel_requested", params![node_id], &[("id", Type::Uuid),("lease_token", Type::String)]).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                Ok::<_, artifactory_database::Error>(CanceledAttempt {
                    id: row.decode("id")?,
                    lease_token: row.decode("lease_token")?,
                })
            })
            .collect::<artifactory_database::Result<_>>()?)
    }

    pub(crate) async fn pending_publications(&self) -> Result<Vec<Uuid>> {
        let pending:Vec<Uuid>=sql::scalars(&mut self.pool.clone(), "SELECT id FROM builds WHERE status='succeeded' AND publish_status='pending' AND NOT cancel_requested LIMIT 10", params![]).await?;
        Ok(pending)
    }

    pub(crate) async fn claim_job(
        &self,
        node_id: Uuid,
        session: Uuid,
    ) -> Result<Option<LeasedJob>> {
        let mut transaction_db = self.pool.clone();
        let mut transaction = transaction_db.transaction().await?;
        let Some(node)=sql::optional_record(&mut transaction, "SELECT * FROM nodes WHERE id=$1 AND active_session=$2 AND NOT revoked AND NOT draining AND NOT ota_draining AND certificate_expires_at>clock_timestamp() AND last_seen>now()-interval '30 seconds' FOR UPDATE SKIP LOCKED", params![node_id, session], &[("id", Type::Uuid),("name", Type::String),("os", Type::String),("arch", Type::String),("pool_id", Type::Uuid),("labels", Type::String),("capabilities", Type::String),("diagnostics", Type::String),("signing_profiles", Type::List(Box::new(Type::String))),("slots", Type::I32),("draining", Type::Bool),("revoked", Type::Bool),("certificate_hash", Type::String),("certificate_expires_at", Type::Timestamp),("active_session", Type::Uuid),("last_seen", Type::Timestamp),("created_at", Type::Timestamp),("emissary_target_release", Type::Uuid),("emissary_current_version", Type::String),("emissary_launcher_version", Type::String),("emissary_ota_status", Type::String),("ota_draining", Type::Bool)]).await? else { return Ok(None); };
        let active:i64=sql::scalar(&mut transaction, "SELECT count(*) FROM attempts WHERE node_id=$1 AND status IN ('leased','running') AND lease_expires_at>now()", params![node_id]).await?;
        if active >= node.decode::<i32>("slots")? as i64 {
            return Ok(None);
        }
        let candidates=sql::records(&mut transaction, "SELECT j.id,j.target_config,j.plan_revision_id,b.source_snapshot_id,b.goal,b.planning_context,p.signing_profiles AS permitted_signing FROM jobs j JOIN builds b ON b.id=j.build_id JOIN projects p ON p.id=b.project_id WHERE j.status='queued' AND b.status IN ('queued','running') AND NOT b.cancel_requested AND $1=ANY(p.pool_ids) ORDER BY j.created_at FOR UPDATE OF j SKIP LOCKED LIMIT 128", params![node.decode::<Uuid>("pool_id")?], &[("id", Type::Uuid),("target_config", Type::String),("plan_revision_id", Type::Uuid),("source_snapshot_id", Type::Uuid),("goal", Type::String),("planning_context", Type::String),("permitted_signing", Type::List(Box::new(Type::String)))]).await?;
        for candidate in candidates {
            if candidate.decode::<Option<String>>("goal")?.is_some() {
                let capabilities: Value = node.decode("capabilities")?;
                if !["source-cache-v1", "external-build-plan-v1"]
                    .iter()
                    .all(|required| {
                        capabilities.as_array().is_some_and(|items| {
                            items.iter().any(|item| item.as_str() == Some(required))
                        })
                    })
                {
                    continue;
                }
                let context: Value = candidate.decode("planning_context")?;
                let allowed: Vec<Uuid> =
                    serde_json::from_value(context.get("pool_ids").cloned().unwrap_or(json!([])))?;
                if !allowed.contains(&node.decode("pool_id")?) {
                    continue;
                }
            }
            let target: Target = serde_json::from_value(candidate.decode("target_config")?)?;
            let permitted: Vec<String> = candidate.decode("permitted_signing")?;
            if !target
                .signing_profiles
                .iter()
                .all(|profile| permitted.contains(profile))
            {
                continue;
            }
            if !crate::scheduler::matches(
                &target,
                &node.decode::<String>("os")?,
                &node.decode::<String>("arch")?,
                &node.decode("labels")?,
                &node.decode("capabilities")?,
                &node.decode::<Vec<String>>("signing_profiles")?,
            ) {
                continue;
            }
            let job: Uuid = candidate.decode("id")?;
            let attempt = Uuid::new_v4();
            let command_id = Uuid::new_v4();
            let lease_token = crypto::token();
            sql::execute(&mut transaction, "INSERT INTO attempts(id,job_id,node_id,lease_token,lease_expires_at,status,command_id,plan_revision_id,source_snapshot_id) VALUES($1,$2,$3,$4,now()+interval '90 seconds','leased',$5,$6,$7)", params![attempt, job, node_id, &lease_token, command_id, candidate.decode::<Option<Uuid>>("plan_revision_id")?, candidate.decode::<Option<Uuid>>("source_snapshot_id")?]).await?;
            sql::execute(
                &mut transaction,
                "UPDATE jobs SET status='leased' WHERE id=$1",
                params![job],
            )
            .await?;
            transaction.commit().await?;
            return Ok(Some(LeasedJob {
                job,
                attempt,
                command_id,
                lease_token,
            }));
        }
        Ok(None)
    }

    pub(crate) async fn assignment_failed(&self, job: Uuid, attempt: Uuid) -> Result<()> {
        sql::execute(&mut self.pool.clone(), "UPDATE attempts SET status='failed',failure_kind='source',error='platform source preparation failed',finished_at=now() WHERE id=$1 AND status='leased'", params![attempt]).await?;
        sql::execute(
            &mut self.pool.clone(),
            "UPDATE jobs SET status='failed' WHERE id=$1 AND status='leased'",
            params![job],
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn assignment_context(&self, job: Uuid) -> Result<AssignmentContext> {
        let row=sql::record(&mut self.pool.clone(), "SELECT j.target,j.target_config,j.plan_revision_id,b.source_snapshot_id,b.id,b.commit_sha,b.config_snapshot,b.parameters,b.source_repository_url AS repository_url,b.source_credential_id AS credential_id FROM jobs j JOIN builds b ON b.id=j.build_id WHERE j.id=$1", params![job], &[("target", Type::String),("target_config", Type::String),("plan_revision_id", Type::Uuid),("source_snapshot_id", Type::Uuid),("id", Type::Uuid),("commit_sha", Type::String),("config_snapshot", Type::String),("parameters", Type::String),("repository_url", Type::String),("credential_id", Type::Uuid)]).await?;
        let plan_id: Option<Uuid> = row.decode("plan_revision_id")?;
        let plan = if let Some(id) = plan_id {
            sql::optional_scalar(
                &mut self.pool.clone(),
                "SELECT contents FROM plan_revisions WHERE id=$1",
                params![id],
            )
            .await?
        } else {
            None
        };
        Ok(AssignmentContext {
            target: row.decode("target")?,
            target_config: row.decode("target_config")?,
            plan_revision_id: row.decode("plan_revision_id")?,
            source_snapshot_id: row.decode("source_snapshot_id")?,
            id: row.decode("id")?,
            commit_sha: row.decode("commit_sha")?,
            config_snapshot: row.decode("config_snapshot")?,
            parameters: row.decode("parameters")?,
            repository_url: row.decode("repository_url")?,
            credential_id: row.decode("credential_id")?,
            plan,
        })
    }

    pub(crate) async fn resolve_build<F, Fut>(&self, prepare: F) -> Result<()>
    where
        F: FnOnce(ResolutionInput) -> Fut,
        Fut: std::future::Future<Output = Result<ResolvedSource>>,
    {
        let mut resolver_lock_db = self.pool.clone();
        let mut resolver_lock = resolver_lock_db.transaction().await?;
        let acquired: bool = sql::scalar(
            &mut resolver_lock,
            "SELECT pg_try_advisory_xact_lock(hashtextextended('artifactory:source-resolver',2))",
            params![],
        )
        .await?;
        if !acquired {
            return Ok(());
        }
        let Some(row) = sql::optional_record(&mut self.pool.clone(), "SELECT b.id,b.project_id,b.requested_ref,b.parameters,b.selected_targets,b.release_version,b.goal,b.planning_context,b.model_config_snapshot,b.source_repository_url AS repository_url,b.source_credential_id AS credential_id,p.signing_profiles FROM builds b JOIN projects p ON p.id=b.project_id WHERE b.status='resolving' AND b.planner_backend='poll' AND NOT b.cancel_requested ORDER BY b.created_at LIMIT 1", params![], &[("id", Type::Uuid),("project_id", Type::Uuid),("requested_ref", Type::String),("parameters", Type::String),("selected_targets", Type::List(Box::new(Type::String))),("release_version", Type::String),("goal", Type::String),("planning_context", Type::String),("model_config_snapshot", Type::String),("repository_url", Type::String),("credential_id", Type::Uuid),("signing_profiles", Type::List(Box::new(Type::String)))]).await? else { return Ok(()); };
        let id: Uuid = row.decode("id")?;
        let input = ResolutionInput {
            project_id: row.decode("project_id")?,
            requested_ref: row.decode("requested_ref")?,
            parameters: row.decode("parameters")?,
            selected_targets: row.decode("selected_targets")?,
            release_version: row.decode("release_version")?,
            goal: row.decode("goal")?,
            planning_context: row.decode("planning_context")?,
            model_config_snapshot: row.decode("model_config_snapshot")?,
            repository_url: row.decode("repository_url")?,
            credential_id: row.decode("credential_id")?,
            signing_profiles: row.decode("signing_profiles")?,
        };
        let result = async {
        match prepare(input).await? {
            ResolvedSource::Planned { commit, snapshot_id, model } => {
                let mut tx_db = self.pool.clone();
let mut tx = tx_db.transaction().await?;
                let status: String = sql::scalar(&mut tx, "SELECT status FROM builds WHERE id=$1 FOR UPDATE", params![id]).await?;
                if status != "resolving" { return Ok(()); }
                sql::execute(&mut tx, "UPDATE builds SET commit_sha=$2,source_snapshot_id=$3,status='planning' WHERE id=$1", params![id, commit, snapshot_id]).await?;
                super::models::PlanningRun::create().id(Uuid::new_v4()).build_id(id).purpose("initial").model_config(toasty::Json((model).clone())).exec(&mut tx).await?;
                tx.commit().await?;
            }
            ResolvedSource::Recipe { commit, yaml, recipe, targets } => {
                let mut transaction_db = self.pool.clone();
let mut transaction = transaction_db.transaction().await?;
                let canceled: bool = sql::scalar(&mut transaction, "SELECT cancel_requested FROM builds WHERE id=$1 FOR UPDATE", params![id]).await?;
                if canceled { return Ok(()); }
                for (name, target) in targets {
                    super::models::Job::upsert_by_build_id_and_target(id,name).id(Uuid::new_v4()).target_config(toasty::Json((serde_json::to_value(target)?).clone())).or_ignore().exec(&mut transaction).await?;
                }
                sql::execute(&mut transaction, "UPDATE builds SET commit_sha=$2,config_yaml=$3,config_snapshot=$4,status='queued' WHERE id=$1", params![id, commit, yaml, serde_json::to_value(recipe)?]).await?;
                transaction.commit().await?;
            }
        }
        Ok::<(), anyhow::Error>(())
    }.await;
        if let Err(error) = result {
            sql::execute(&mut self.pool.clone(), "UPDATE builds SET status='failed',error=$2,finished_at=now() WHERE id=$1 AND status='resolving' AND NOT cancel_requested", params![id, error.to_string()]).await?;
        }
        // Keep the resolver lock until preparation and the final state update finish.
        drop(resolver_lock);
        Ok(())
    }
}
