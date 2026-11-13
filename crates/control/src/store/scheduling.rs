//! Scheduler persistence. Claim, publish and resolver lock lifetimes are owned here.
use super::ControlStore;
use crate::{
    crypto,
    recipe::{Recipe, Target},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sqlx::Row;
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
        let mut transaction = self.pool.begin().await?;
        let rows=sqlx::query("SELECT a.id,a.job_id,a.status,b.cancel_requested FROM attempts a JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE a.status IN ('leased','running') AND a.lease_expires_at<=now() FOR UPDATE OF a,j SKIP LOCKED")
        .fetch_all(&mut *transaction).await?;
        for row in rows {
            let started = row.get::<String, _>("status") == "running";
            let canceled: bool = row.get("cancel_requested");
            sqlx::query("UPDATE attempts SET status=$2,error='execution lease expired',finished_at=now() WHERE id=$1")
            .bind(row.get::<Uuid,_>("id")).bind(if canceled {"canceled"} else {"lost"}).execute(&mut *transaction).await?;
            sqlx::query("UPDATE jobs SET status=$2 WHERE id=$1")
                .bind(row.get::<Uuid, _>("job_id"))
                .bind(if canceled {
                    "canceled"
                } else if started {
                    "lost"
                } else {
                    "queued"
                })
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn refresh_builds(&self) -> Result<()> {
        sqlx::query("WITH summaries AS (SELECT b.id, CASE WHEN b.cancel_requested THEN CASE WHEN bool_or(j.status IN ('leased','running')) THEN 'canceling' ELSE 'canceled' END WHEN bool_or(j.status='running') THEN 'running' WHEN bool_or(j.status IN ('queued','leased')) THEN 'queued' WHEN bool_or(j.status='failed') THEN 'failed' WHEN bool_or(j.status='lost') THEN 'lost' WHEN bool_or(j.status='canceled') THEN 'canceled' ELSE 'succeeded' END AS status FROM builds b JOIN jobs j ON j.build_id=b.id WHERE b.status NOT IN ('resolving','planning','replanning','needs_input') GROUP BY b.id) UPDATE builds b SET status=s.status,finished_at=CASE WHEN s.status IN ('succeeded','failed','lost','canceled') THEN coalesce(b.finished_at,now()) ELSE NULL END FROM summaries s WHERE b.id=s.id")
        .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn queue_revisions(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let rows=sqlx::query("SELECT id,model_config_snapshot FROM builds b WHERE goal IS NOT NULL AND current_plan_id IS NOT NULL AND status='failed' AND NOT cancel_requested AND revision_count<2 AND (SELECT state FROM planning_runs WHERE build_id=b.id ORDER BY created_at DESC LIMIT 1)='succeeded' AND EXISTS(SELECT 1 FROM jobs j JOIN LATERAL (SELECT failure_kind FROM attempts WHERE job_id=j.id ORDER BY created_at DESC LIMIT 1) a ON true WHERE j.build_id=b.id AND j.status='failed' AND a.failure_kind='execution') ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT 10")
        .fetch_all(&mut *tx).await?;
        for row in rows {
            let id: Uuid = row.get("id");
            let failed:Vec<String>=sqlx::query_scalar("SELECT j.target FROM jobs j JOIN LATERAL (SELECT failure_kind FROM attempts WHERE job_id=j.id ORDER BY created_at DESC LIMIT 1) a ON true WHERE j.build_id=$1 AND j.status='failed' AND a.failure_kind='execution'").bind(id).fetch_all(&mut *tx).await?;
            sqlx::query("UPDATE builds SET revision_count=revision_count+1,status='replanning',finished_at=NULL WHERE id=$1").bind(id).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO planning_runs(id,build_id,purpose,model_config,failed_targets) VALUES($1,$2,'revision',$3,$4)").bind(Uuid::new_v4()).bind(id).bind(row.get::<Value,_>("model_config_snapshot")).bind(failed).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn publish_transaction(&self, id: Uuid) -> Result<Uuid> {
        let mut transaction = self.pool.begin().await?;
        let build = sqlx::query("SELECT * FROM builds WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *transaction)
            .await?;
        ensure!(
            build.get::<String, _>("status") == "succeeded"
                && !build.get::<bool, _>("cancel_requested"),
            "only a successful build can be published"
        );
        let version: Option<String> = build.get("release_version");
        let version = version.ok_or_else(|| anyhow::anyhow!("release version is required"))?;
        let recipe: Recipe = serde_json::from_value(build.get("config_snapshot"))?;
        ensure!(
            recipe.release.enabled,
            "publishing is disabled by repository configuration"
        );
        let key = format!("build:{id}");
        let project: Uuid = build.get("project_id");
        if let Some(existing) = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM releases WHERE project_id=$1 AND idempotency_key=$2",
        )
        .bind(project)
        .bind(&key)
        .fetch_optional(&mut *transaction)
        .await?
        {
            return Ok(existing);
        }
        let artifacts:Vec<Uuid>=sqlx::query_scalar("SELECT ar.id FROM artifacts ar JOIN attempts a ON a.id=ar.attempt_id JOIN jobs j ON j.id=a.job_id WHERE j.build_id=$1 AND j.status='succeeded' AND a.status='succeeded' AND a.plan_revision_id IS NOT DISTINCT FROM j.plan_revision_id AND a.source_snapshot_id IS NOT DISTINCT FROM (SELECT source_snapshot_id FROM builds WHERE id=$1) AND a.id=(SELECT id FROM attempts WHERE job_id=j.id ORDER BY created_at DESC,id DESC LIMIT 1)").bind(id).fetch_all(&mut *transaction).await?;
        let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE build_id=$1")
            .bind(id)
            .fetch_one(&mut *transaction)
            .await?;
        let complete:i64=sqlx::query_scalar("SELECT count(DISTINCT j.id) FROM jobs j JOIN attempts a ON a.job_id=j.id AND a.status='succeeded' JOIN artifacts ar ON ar.attempt_id=a.id WHERE j.build_id=$1 AND j.status='succeeded' AND ar.id=ANY($2)").bind(id).bind(&artifacts).fetch_one(&mut *transaction).await?;
        ensure!(
            jobs > 0 && jobs == complete,
            "every target must have verified artifacts"
        );
        let release = Uuid::new_v4();
        sqlx::query("INSERT INTO releases(id,project_id,build_id,version,idempotency_key,commit_sha) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(release).bind(project).bind(id).bind(version).bind(key).bind(build.get::<String,_>("commit_sha")).execute(&mut *transaction).await.context("release version already exists or publication failed")?;
        for artifact in artifacts {
            sqlx::query("INSERT INTO release_artifacts VALUES($1,$2)")
                .bind(release)
                .bind(artifact)
                .execute(&mut *transaction)
                .await?;
        }
        sqlx::query("UPDATE builds SET publish_status='succeeded',publish_error=NULL WHERE id=$1")
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("INSERT INTO publish_attempts(id,build_id,status) VALUES($1,$2,'succeeded')")
            .bind(Uuid::new_v4())
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(release)
    }

    pub(crate) async fn publication_failed(&self, id: Uuid, error: &str) -> Result<()> {
        sqlx::query("UPDATE builds SET publish_status='failed',publish_error=$2 WHERE id=$1 AND publish_status<>'succeeded'").bind(id).bind(error).execute(&self.pool).await?;
        sqlx::query(
            "INSERT INTO publish_attempts(id,build_id,status,error) VALUES($1,$2,'failed',$3)",
        )
        .bind(Uuid::new_v4())
        .bind(id)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn canceled_attempts(&self, node_id: Uuid) -> Result<Vec<CanceledAttempt>> {
        let rows=sqlx::query("SELECT a.id,a.lease_token FROM attempts a JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE a.node_id=$1 AND a.status IN ('leased','running') AND b.cancel_requested")
            .bind(node_id).fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|row| CanceledAttempt {
                id: row.get("id"),
                lease_token: row.get("lease_token"),
            })
            .collect())
    }

    pub(crate) async fn pending_publications(&self) -> Result<Vec<Uuid>> {
        let pending:Vec<Uuid>=sqlx::query_scalar("SELECT id FROM builds WHERE status='succeeded' AND publish_status='pending' AND NOT cancel_requested LIMIT 10").fetch_all(&self.pool).await?;
        Ok(pending)
    }

    pub(crate) async fn claim_job(
        &self,
        node_id: Uuid,
        session: Uuid,
    ) -> Result<Option<LeasedJob>> {
        let mut transaction = self.pool.begin().await?;
        let Some(node)=sqlx::query("SELECT * FROM nodes WHERE id=$1 AND active_session=$2 AND NOT revoked AND NOT draining AND NOT ota_draining AND certificate_expires_at>clock_timestamp() AND last_seen>now()-interval '30 seconds' FOR UPDATE SKIP LOCKED")
        .bind(node_id).bind(session).fetch_optional(&mut *transaction).await? else { return Ok(None); };
        let active:i64=sqlx::query_scalar("SELECT count(*) FROM attempts WHERE node_id=$1 AND status IN ('leased','running') AND lease_expires_at>now()")
        .bind(node_id).fetch_one(&mut *transaction).await?;
        if active >= node.get::<i32, _>("slots") as i64 {
            return Ok(None);
        }
        let candidates=sqlx::query("SELECT j.id,j.target_config,j.plan_revision_id,b.source_snapshot_id,b.goal,b.planning_context,p.signing_profiles AS permitted_signing FROM jobs j JOIN builds b ON b.id=j.build_id JOIN projects p ON p.id=b.project_id WHERE j.status='queued' AND b.status IN ('queued','running') AND NOT b.cancel_requested AND $1=ANY(p.pool_ids) ORDER BY j.created_at FOR UPDATE OF j SKIP LOCKED LIMIT 128")
        .bind(node.get::<Uuid,_>("pool_id")).fetch_all(&mut *transaction).await?;
        for candidate in candidates {
            if candidate.get::<Option<String>, _>("goal").is_some() {
                let capabilities: Value = node.get("capabilities");
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
                let context: Value = candidate.get("planning_context");
                let allowed: Vec<Uuid> =
                    serde_json::from_value(context.get("pool_ids").cloned().unwrap_or(json!([])))?;
                if !allowed.contains(&node.get("pool_id")) {
                    continue;
                }
            }
            let target: Target = serde_json::from_value(candidate.get("target_config"))?;
            let permitted: Vec<String> = candidate.get("permitted_signing");
            if !target
                .signing_profiles
                .iter()
                .all(|profile| permitted.contains(profile))
            {
                continue;
            }
            if !crate::scheduler::matches(
                &target,
                &node.get::<String, _>("os"),
                &node.get::<String, _>("arch"),
                &node.get("labels"),
                &node.get("capabilities"),
                &node.get::<Vec<String>, _>("signing_profiles"),
            ) {
                continue;
            }
            let job: Uuid = candidate.get("id");
            let attempt = Uuid::new_v4();
            let command_id = Uuid::new_v4();
            let lease_token = crypto::token();
            sqlx::query("INSERT INTO attempts(id,job_id,node_id,lease_token,lease_expires_at,status,command_id,plan_revision_id,source_snapshot_id) VALUES($1,$2,$3,$4,now()+interval '90 seconds','leased',$5,$6,$7)")
            .bind(attempt).bind(job).bind(node_id).bind(&lease_token).bind(command_id).bind(candidate.get::<Option<Uuid>,_>("plan_revision_id")).bind(candidate.get::<Option<Uuid>,_>("source_snapshot_id")).execute(&mut *transaction).await?;
            sqlx::query("UPDATE jobs SET status='leased' WHERE id=$1")
                .bind(job)
                .execute(&mut *transaction)
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
        sqlx::query("UPDATE attempts SET status='failed',failure_kind='source',error='platform source preparation failed',finished_at=now() WHERE id=$1 AND status='leased'").bind(attempt).execute(&self.pool).await?;
        sqlx::query("UPDATE jobs SET status='failed' WHERE id=$1 AND status='leased'")
            .bind(job)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub(crate) async fn assignment_context(&self, job: Uuid) -> Result<AssignmentContext> {
        let row=sqlx::query("SELECT j.target,j.target_config,j.plan_revision_id,b.source_snapshot_id,b.id,b.commit_sha,b.config_snapshot,b.parameters,b.source_repository_url AS repository_url,b.source_credential_id AS credential_id FROM jobs j JOIN builds b ON b.id=j.build_id WHERE j.id=$1")
        .bind(job).fetch_one(&self.pool).await?;
        let plan_id: Option<Uuid> = row.get("plan_revision_id");
        let plan = if let Some(id) = plan_id {
            sqlx::query_scalar("SELECT contents FROM plan_revisions WHERE id=$1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?
        } else {
            None
        };
        Ok(AssignmentContext {
            target: row.get("target"),
            target_config: row.get("target_config"),
            plan_revision_id: row.get("plan_revision_id"),
            source_snapshot_id: row.get("source_snapshot_id"),
            id: row.get("id"),
            commit_sha: row.get("commit_sha"),
            config_snapshot: row.get("config_snapshot"),
            parameters: row.get("parameters"),
            repository_url: row.get("repository_url"),
            credential_id: row.get("credential_id"),
            plan,
        })
    }

    pub(crate) async fn resolve_build<F, Fut>(&self, prepare: F) -> Result<()>
    where
        F: FnOnce(ResolutionInput) -> Fut,
        Fut: std::future::Future<Output = Result<ResolvedSource>>,
    {
        let mut resolver_lock = self.pool.begin().await?;
        let acquired: bool = sqlx::query_scalar(
            "SELECT pg_try_advisory_xact_lock(hashtextextended('artifactory:source-resolver',2))",
        )
        .fetch_one(&mut *resolver_lock)
        .await?;
        if !acquired {
            return Ok(());
        }
        let Some(row) = sqlx::query("SELECT b.id,b.project_id,b.requested_ref,b.parameters,b.selected_targets,b.release_version,b.goal,b.planning_context,b.model_config_snapshot,b.source_repository_url AS repository_url,b.source_credential_id AS credential_id,p.signing_profiles FROM builds b JOIN projects p ON p.id=b.project_id WHERE b.status='resolving' AND b.planner_backend='poll' AND NOT b.cancel_requested ORDER BY b.created_at LIMIT 1").fetch_optional(&self.pool).await? else { return Ok(()); };
        let id: Uuid = row.get("id");
        let input = ResolutionInput {
            project_id: row.get("project_id"),
            requested_ref: row.get("requested_ref"),
            parameters: row.get("parameters"),
            selected_targets: row.get("selected_targets"),
            release_version: row.get("release_version"),
            goal: row.get("goal"),
            planning_context: row.get("planning_context"),
            model_config_snapshot: row.get("model_config_snapshot"),
            repository_url: row.get("repository_url"),
            credential_id: row.get("credential_id"),
            signing_profiles: row.get("signing_profiles"),
        };
        let result = async {
        match prepare(input).await? {
            ResolvedSource::Planned { commit, snapshot_id, model } => {
                let mut tx = self.pool.begin().await?;
                let status: String = sqlx::query_scalar("SELECT status FROM builds WHERE id=$1 FOR UPDATE").bind(id).fetch_one(&mut *tx).await?;
                if status != "resolving" { return Ok(()); }
                sqlx::query("UPDATE builds SET commit_sha=$2,source_snapshot_id=$3,status='planning' WHERE id=$1").bind(id).bind(commit).bind(snapshot_id).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO planning_runs(id,build_id,purpose,model_config) VALUES($1,$2,'initial',$3)").bind(Uuid::new_v4()).bind(id).bind(model).execute(&mut *tx).await?;
                tx.commit().await?;
            }
            ResolvedSource::Recipe { commit, yaml, recipe, targets } => {
                let mut transaction = self.pool.begin().await?;
                let canceled: bool = sqlx::query_scalar("SELECT cancel_requested FROM builds WHERE id=$1 FOR UPDATE").bind(id).fetch_one(&mut *transaction).await?;
                if canceled { return Ok(()); }
                for (name, target) in targets {
                    sqlx::query("INSERT INTO jobs(id,build_id,target,target_config) VALUES($1,$2,$3,$4) ON CONFLICT(build_id,target) DO NOTHING")
                        .bind(Uuid::new_v4()).bind(id).bind(name).bind(serde_json::to_value(target)?).execute(&mut *transaction).await?;
                }
                sqlx::query("UPDATE builds SET commit_sha=$2,config_yaml=$3,config_snapshot=$4,status='queued' WHERE id=$1")
                    .bind(id).bind(commit).bind(yaml).bind(serde_json::to_value(recipe)?).execute(&mut *transaction).await?;
                transaction.commit().await?;
            }
        }
        Ok::<(), anyhow::Error>(())
    }.await;
        if let Err(error) = result {
            sqlx::query("UPDATE builds SET status='failed',error=$2,finished_at=now() WHERE id=$1 AND status='resolving' AND NOT cancel_requested")
            .bind(id).bind(error.to_string()).execute(&self.pool).await?;
        }
        // Keep the resolver lock until preparation and the final state update finish.
        drop(resolver_lock);
        Ok(())
    }
}
