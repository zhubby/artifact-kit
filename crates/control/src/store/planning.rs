//! Planning leases, budgets, revisions and their atomic state transitions.
use super::ControlStore;
use crate::{crypto, execution_plan::ExecutionPlan, pki};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use sqlx::{Acquire, Row, postgres::PgRow};
use tonic::Status;
use uuid::Uuid;
fn internal(error: impl std::fmt::Display) -> Status {
    crate::planning::internal(error)
}
pub(crate) struct PlanningClaim {
    pub id: Uuid,
    pub token: String,
    pub config: Value,
    pub context: Value,
    pub timeout_seconds: u32,
    pub max_calls: u32,
}
pub(crate) struct PlanningLog {
    pub object_key: String,
    pub step: String,
}
pub(crate) struct SubmissionContext {
    pub snapshot_id: Uuid,
    pub parameters: Value,
    pub target_requirements: Value,
    pub planning_context: Value,
    pub selected_targets: Vec<String>,
    pub current_plan_id: Option<Uuid>,
    pub failed_targets: Vec<String>,
    pub previous: Option<Value>,
}
pub(crate) enum PlanningAction {
    Submit(SubmissionContext),
    ReadLogs(Uuid),
}
pub(crate) enum PlanningPrepared {
    Plan(ExecutionPlan),
    Value(Value),
}
pub(crate) enum PlanningToolResult {
    Value(Value),
    Inspect(Uuid),
}
async fn lock_run(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    worker: Uuid,
    id: &str,
    token: &str,
) -> std::result::Result<PgRow, Status> {
    let id = Uuid::parse_str(id).map_err(|_| Status::invalid_argument("invalid planning run"))?;
    let build: Uuid = sqlx::query_scalar("SELECT build_id FROM planning_runs WHERE id=$1")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(internal)?
        .ok_or_else(|| Status::not_found("planning run not found"))?;
    sqlx::query("SELECT id FROM builds WHERE id=$1 FOR UPDATE")
        .bind(build)
        .execute(&mut **tx)
        .await
        .map_err(internal)?;
    sqlx::query("SELECT id FROM planning_runs WHERE id=$1 FOR UPDATE")
        .bind(id)
        .execute(&mut **tx)
        .await
        .map_err(internal)?;
    sqlx::query("SELECT id FROM ai_workers WHERE id=$1 FOR SHARE")
        .bind(worker)
        .execute(&mut **tx)
        .await
        .map_err(internal)?;
    sqlx::query("SELECT r.*,b.project_id,b.source_snapshot_id,b.goal,b.target_requirements,b.planning_context,b.parameters,b.selected_targets,b.current_plan_id,b.revision_count,b.release_version FROM planning_runs r JOIN builds b ON b.id=r.build_id JOIN ai_workers w ON w.id=r.worker_id WHERE r.id=$1 AND r.worker_id=$2 AND r.lease_hash=$3 AND r.state='leased' AND r.lease_expires_at>clock_timestamp() AND r.deadline_at>clock_timestamp() AND NOT b.cancel_requested AND b.status IN ('planning','replanning') AND NOT w.revoked AND w.certificate_expires_at>clock_timestamp()")
        .bind(id).bind(worker).bind(crypto::hash(token)).fetch_optional(&mut **tx).await.map_err(internal)?.ok_or_else(||Status::failed_precondition("planning lease expired, canceled or superseded"))
}

pub(super) async fn submission_context(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    row: &PgRow,
) -> Result<SubmissionContext> {
    let current_plan_id: Option<Uuid> = row.get("current_plan_id");
    let previous = if let Some(id) = current_plan_id {
        Some(
            sqlx::query_scalar("SELECT contents FROM plan_revisions WHERE id=$1")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?,
        )
    } else {
        None
    };
    Ok(SubmissionContext {
        snapshot_id: row.get("source_snapshot_id"),
        parameters: row.get("parameters"),
        target_requirements: row.get("target_requirements"),
        planning_context: row.get("planning_context"),
        selected_targets: row.get("selected_targets"),
        current_plan_id,
        failed_targets: row.get("failed_targets"),
        previous,
    })
}
pub(super) async fn persist_plan(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    row: &PgRow,
    plan: ExecutionPlan,
) -> Result<Value> {
    let build: Uuid = row.get("build_id");
    let snapshot: Uuid = row.get("source_snapshot_id");
    let current: Option<Uuid> = row.get("current_plan_id");
    let failed: Vec<String> = row.get("failed_targets");
    let plan_id = Uuid::new_v4();
    let contents = serde_json::to_value(&plan)?;
    sqlx::query("INSERT INTO plan_revisions(id,build_id,run_id,revision,source_snapshot_id,contents,sha256) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(plan_id).bind(build).bind(row.get::<Uuid,_>("id")).bind(row.get::<i32,_>("revision_count")).bind(snapshot).bind(&contents).bind(crypto::hash(serde_json::to_vec(&contents)?)).execute(&mut **tx).await?;
    for (name, target) in &plan.recipe.targets {
        if current.is_none() {
            sqlx::query("INSERT INTO jobs(id,build_id,target,target_config,plan_revision_id) VALUES($1,$2,$3,$4,$5)").bind(Uuid::new_v4()).bind(build).bind(name).bind(serde_json::to_value(target)?).bind(plan_id).execute(&mut **tx).await?;
        } else if failed.contains(name) {
            let changed=sqlx::query("UPDATE jobs SET target_config=$3,plan_revision_id=$4,status='queued' WHERE build_id=$1 AND target=$2 AND status IN ('failed','lost')")
                .bind(build).bind(name).bind(serde_json::to_value(target)?).bind(plan_id).execute(&mut **tx).await?;
            ensure!(
                changed.rows_affected() == 1,
                "target is no longer eligible for revision"
            );
        }
    }
    sqlx::query("UPDATE builds SET current_plan_id=$2,config_snapshot=$3,status='queued',error=NULL,finished_at=NULL WHERE id=$1").bind(build).bind(plan_id).bind(serde_json::to_value(&plan.recipe)?).execute(&mut **tx).await?;
    sqlx::query("UPDATE planning_runs SET state='succeeded',finished_at=now() WHERE id=$1")
        .bind(row.get::<Uuid, _>("id"))
        .execute(&mut **tx)
        .await?;
    Ok(json!({"accepted":true,"plan_revision_id":plan_id}))
}
impl ControlStore {
    pub(crate) async fn planner_identity(&self, hash: String) -> std::result::Result<Uuid, Status> {
        sqlx::query_scalar("SELECT id FROM ai_workers WHERE certificate_hash=$1 AND NOT revoked AND certificate_expires_at>clock_timestamp()")
        .bind(hash).fetch_optional(&self.pool).await.map_err(internal)?.ok_or_else(||Status::unauthenticated("AI worker revoked, expired or unknown"))
    }
    pub(crate) async fn enroll_planner<F, Fut>(
        &self,
        token_value: &str,
        name: &str,
        issue: F,
    ) -> std::result::Result<super::executions::EnrolledNode, Status>
    where
        F: FnOnce(Uuid) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<String, Status>>,
    {
        let mut tx = self.pool.begin().await.map_err(internal)?;
        let token:Option<Uuid>=sqlx::query_scalar("SELECT id FROM ai_enrollment_tokens WHERE token_hash=$1 AND used_at IS NULL AND expires_at>clock_timestamp() FOR UPDATE").bind(crypto::hash(token_value)).fetch_optional(&mut *tx).await.map_err(internal)?;
        let token = token.ok_or_else(|| Status::unauthenticated("invalid enrollment token"))?;
        let id = Uuid::new_v4();
        let certificate = issue(id).await?;
        sqlx::query("INSERT INTO ai_workers(id,name,certificate_hash,certificate_expires_at) VALUES($1,$2,$3,now()+interval '29 days')").bind(id).bind(name).bind(pki::certificate_hash(&certificate).map_err(internal)?).execute(&mut *tx).await.map_err(internal)?;
        sqlx::query("UPDATE ai_enrollment_tokens SET used_at=now() WHERE id=$1")
            .bind(token)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(super::executions::EnrolledNode { id, certificate })
    }
    pub(crate) async fn planning_nodes(&self, build: Uuid) -> Result<Vec<Value>> {
        let context: Value = sqlx::query_scalar("SELECT planning_context FROM builds WHERE id=$1")
            .bind(build)
            .fetch_one(&self.pool)
            .await?;
        let pools: Vec<Uuid> =
            serde_json::from_value(context.get("pool_ids").cloned().unwrap_or(json!([])))?;
        Ok(sqlx::query_scalar("SELECT jsonb_build_object('name',n.name,'os',os,'arch',arch,'labels',labels,'capabilities',capabilities,'signing_profiles',signing_profiles,'tools',diagnostics->'tools','online',last_seen>now()-interval '30 seconds') FROM nodes n WHERE NOT revoked AND pool_id=ANY($1)")
        .bind(pools).fetch_all(&self.pool).await?)
    }
    pub(crate) async fn claim_planning(
        &self,
        worker: Uuid,
    ) -> std::result::Result<Option<PlanningClaim>, Status> {
        let mut tx = self.pool.begin().await.map_err(internal)?;
        let draining: bool = sqlx::query_scalar(
            "UPDATE ai_workers SET last_seen=now() WHERE id=$1 RETURNING draining",
        )
        .bind(worker)
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?;
        let active: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM planning_runs WHERE worker_id=$1 AND state='leased')",
        )
        .bind(worker)
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?;
        if draining || active {
            tx.commit().await.map_err(internal)?;
            return Ok(None);
        }
        let Some(row)=sqlx::query("SELECT r.id,r.build_id,r.model_config,r.calls,r.usage FROM planning_runs r JOIN builds b ON b.id=r.build_id WHERE r.state='queued' AND r.authority='poll' AND NOT b.cancel_requested AND b.status IN ('planning','replanning') ORDER BY r.created_at FOR UPDATE OF r SKIP LOCKED LIMIT 1").fetch_optional(&mut *tx).await.map_err(internal)? else { tx.commit().await.map_err(internal)?; return Ok(None); };
        let id: Uuid = row.get("id");
        let build: Uuid = row.get("build_id");
        let config: Value = row.get("model_config");
        let timeout = config["timeout_seconds"].as_i64().unwrap_or(600);
        let token = crypto::token();
        let remaining:i64=sqlx::query_scalar("UPDATE planning_runs SET worker_id=$2,state='leased',lease_hash=$3,deadline_at=coalesce(deadline_at,clock_timestamp()+make_interval(secs=>$4)),lease_expires_at=clock_timestamp()+interval '90 seconds' WHERE id=$1 RETURNING greatest(1,extract(epoch FROM deadline_at-clock_timestamp())::bigint)")
            .bind(id).bind(worker).bind(crypto::hash(&token)).bind(timeout as f64).fetch_one(&mut *tx).await.map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        let b=sqlx::query("SELECT goal,requested_ref,commit_sha,parameters,target_requirements,planning_context,source_snapshot_id,revision_count,clarification FROM builds WHERE id=$1").bind(build).fetch_one(&self.pool).await.map_err(internal)?;
        let current:Option<Value>=sqlx::query_scalar("SELECT contents FROM plan_revisions WHERE id=(SELECT current_plan_id FROM builds WHERE id=$1)").bind(build).fetch_optional(&self.pool).await.map_err(internal)?;
        let failed:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('target',j.target,'error',a.error,'attempt_id',a.id) FROM jobs j JOIN LATERAL (SELECT id,error FROM attempts WHERE job_id=j.id ORDER BY created_at DESC LIMIT 1) a ON true WHERE j.build_id=$1 AND j.status IN ('failed','lost')").bind(build).fetch_all(&self.pool).await.map_err(internal)?;
        let checkpoint:Option<Value>=sqlx::query_scalar("SELECT payload FROM planning_events WHERE run_id=$1 AND kind='checkpoint' ORDER BY id DESC LIMIT 1").bind(id).fetch_optional(&self.pool).await.map_err(internal)?;
        let context = json!({"build_id":build,"goal":b.get::<Option<String>,_>("goal"),"ref":b.get::<String,_>("requested_ref"),"commit":b.get::<Option<String>,_>("commit_sha"),"parameters":b.get::<Value,_>("parameters"),"target_requirements":b.get::<Value,_>("target_requirements"),"project_context":b.get::<Value,_>("planning_context"),"snapshot_id":b.get::<Uuid,_>("source_snapshot_id"),"revision":b.get::<i32,_>("revision_count"),"current_plan":current,"failed_targets":failed,"nodes":self.planning_nodes(build).await.map_err(internal)?,"checkpoint":checkpoint,"usage":row.get::<Value,_>("usage")});
        Ok(Some(PlanningClaim {
            id,
            token,
            config: config.clone(),
            context,
            timeout_seconds: remaining.min(timeout) as u32,
            max_calls: (config["max_calls"].as_i64().unwrap_or(20)
                - row.get::<i32, _>("calls") as i64)
                .max(0) as u32,
        }))
    }
    pub(crate) async fn renew_planning(
        &self,
        worker: Uuid,
        run_id: &str,
        token: &str,
    ) -> std::result::Result<bool, Status> {
        let mut tx = self.pool.begin().await.map_err(internal)?;
        if lock_run(&mut tx, worker, run_id, token).await.is_err() {
            return Ok(true);
        }
        sqlx::query("UPDATE planning_runs SET lease_expires_at=least(deadline_at,clock_timestamp()+interval '90 seconds') WHERE id=$1").bind(Uuid::parse_str(run_id).unwrap()).execute(&mut *tx).await.map_err(internal)?;
        sqlx::query("UPDATE ai_workers SET last_seen=now() WHERE id=$1")
            .bind(worker)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(false)
    }
    pub(crate) async fn planning_tool<F, Fut>(
        &self,
        worker: Uuid,
        run_id: &str,
        token: &str,
        name: &str,
        args: &Value,
        prepare: F,
    ) -> std::result::Result<PlanningToolResult, Status>
    where
        F: FnOnce(PlanningAction) -> Fut,
        Fut: std::future::Future<Output = Result<PlanningPrepared>>,
    {
        let mut tx = self.pool.begin().await.map_err(internal)?;
        let row = lock_run(&mut tx, worker, run_id, token).await?;
        let build: Uuid = row.get("build_id");
        let run: Uuid = row.get("id");
        let result: Result<Value> = match name {
            "submit_plan" => {
                // A rejected plan must leave no jobs or revision behind, including
                // failures after the first SQL write. Keep the lease transaction.
                let mut savepoint = tx.begin().await.map_err(internal)?;
                let result = async {
                    let context = submission_context(&mut savepoint, &row).await?;
                    let PlanningPrepared::Plan(plan) =
                        prepare(PlanningAction::Submit(context)).await?
                    else {
                        anyhow::bail!("invalid prepared tool result");
                    };
                    persist_plan(&mut savepoint, &row, plan).await
                }
                .await;
                match result {
                    Ok(value) => {
                        savepoint.commit().await.map_err(internal)?;
                        Ok(value)
                    }
                    Err(error) => {
                        savepoint.rollback().await.map_err(internal)?;
                        Err(error)
                    }
                }
            }
            "request_input" => {
                let question = args.get("question").and_then(Value::as_str).unwrap_or("");
                if question.trim().is_empty() || question.len() > 8000 {
                    Ok(json!({"error":"a concise question is required"}))
                } else {
                    sqlx::query(
                        "UPDATE builds SET status='needs_input',clarification=$2 WHERE id=$1",
                    )
                    .bind(build)
                    .bind(json!({"question":question}))
                    .execute(&mut *tx)
                    .await
                    .map_err(internal)?;
                    sqlx::query("UPDATE planning_runs SET state='needs_input',finished_at=now() WHERE id=$1").bind(run).execute(&mut *tx).await.map_err(internal)?;
                    Ok(json!({"accepted":true,"needs_input":true}))
                }
            }
            "node_capabilities" => self
                .planning_nodes(build)
                .await
                .map(|value| json!({"nodes":value})),
            "read_logs" => match prepare(PlanningAction::ReadLogs(build)).await {
                Ok(PlanningPrepared::Value(value)) => Ok(value),
                Ok(_) => Err(anyhow::anyhow!("invalid prepared tool result")),
                Err(error) => Err(error),
            },
            "list_tree" | "read_file" | "search_source" | "git_metadata" => {
                let snapshot = row.get("source_snapshot_id");
                // Source tools are read-only; do not hold the cancellation lock during I/O.
                tx.commit().await.map_err(internal)?;
                return Ok(PlanningToolResult::Inspect(snapshot));
            }
            _ => Ok(json!({"error":"unknown or prohibited tool"})),
        };
        let value = match result {
            Ok(value) => value,
            Err(error) => json!({"error":error.to_string()}),
        };
        sqlx::query("INSERT INTO planning_events(run_id,kind,payload) VALUES($1,'tool',$2)")
            .bind(run)
            .bind(json!({"name":name,"accepted":value.get("accepted"),"error":value.get("error")}))
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(PlanningToolResult::Value(value))
    }
    pub(crate) async fn planning_report(
        &self,
        worker: Uuid,
        run_id: &str,
        token: &str,
        kind: &str,
        payload_json: &str,
    ) -> std::result::Result<Value, Status> {
        let mut tx = self.pool.begin().await.map_err(internal)?;
        let row = lock_run(&mut tx, worker, run_id, token).await?;
        let run: Uuid = row.get("id");
        let payload: Value = serde_json::from_str(payload_json)
            .map_err(|_| Status::invalid_argument("invalid event JSON"))?;
        let mut wait_ms: i64 = 0;
        if kind == "model_call" {
            let config = row.get::<Value, _>("model_config");
            let limit = config["max_calls"].as_i64().unwrap_or(20);
            if row.get::<i32, _>("calls") as i64 >= limit {
                return Err(Status::resource_exhausted("model call limit reached"));
            }
            if let (Some(profile), Some(rpm)) = (
                config["id"]
                    .as_str()
                    .and_then(|id| Uuid::parse_str(id).ok()),
                config["parameters"]["requests_per_minute"].as_u64(),
            ) && rpm > 0
            {
                // Serialize reservations on the profile, including calls made by
                // other workers. A small margin avoids provider window edges.
                let interval = 60.0 / rpm as f64 + 1.0;
                wait_ms = sqlx::query_scalar("UPDATE model_profiles SET next_call_at=greatest(coalesce(next_call_at,clock_timestamp()),clock_timestamp())+make_interval(secs=>$2) WHERE id=$1 RETURNING greatest(0,ceil(extract(epoch FROM next_call_at-clock_timestamp())*1000-$2*1000)::bigint)")
                    .bind(profile).bind(interval).fetch_optional(&mut *tx).await.map_err(internal)?.unwrap_or(0);
            }
            sqlx::query("UPDATE planning_runs SET calls=calls+1 WHERE id=$1")
                .bind(run)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
        } else if kind == "usage" {
            sqlx::query("UPDATE planning_runs SET usage=$2 WHERE id=$1")
                .bind(run)
                .bind(&payload)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
        } else if kind == "failed" {
            let message = payload
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("AI planning failed")
                .chars()
                .take(2000)
                .collect::<String>();
            sqlx::query(
                "UPDATE planning_runs SET state='failed',error=$2,finished_at=now() WHERE id=$1",
            )
            .bind(run)
            .bind(&message)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
            sqlx::query("UPDATE builds SET status='failed',error=$2,finished_at=now() WHERE id=$1")
                .bind(row.get::<Uuid, _>("build_id"))
                .bind(message)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
        }
        sqlx::query("INSERT INTO planning_events(run_id,kind,payload) VALUES($1,$2,$3)")
            .bind(run)
            .bind(kind)
            .bind(payload)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(json!({"ok":true,"wait_ms":wait_ms}))
    }
    pub(crate) async fn planning_logs(
        &self,
        build: Uuid,
        target: &str,
    ) -> Result<Vec<PlanningLog>> {
        let rows=sqlx::query("SELECT l.object_key,l.step FROM log_chunks l JOIN attempts a ON a.id=l.attempt_id JOIN jobs j ON j.id=a.job_id WHERE j.build_id=$1 AND ($2='' OR j.target=$2) ORDER BY l.id DESC LIMIT 24")
        .bind(build).bind(target).fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|row| PlanningLog {
                object_key: row.get("object_key"),
                step: row.get("step"),
            })
            .collect())
    }
    pub(crate) async fn reap_planning(&self) -> Result<()> {
        let rows=sqlx::query("SELECT r.id,r.build_id FROM planning_runs r LEFT JOIN ai_workers w ON w.id=r.worker_id WHERE r.authority='poll' AND r.state='leased' AND (r.lease_expires_at<=clock_timestamp() OR r.deadline_at<=clock_timestamp() OR w.revoked OR w.certificate_expires_at<=clock_timestamp())")
        .fetch_all(&self.pool).await?;
        for row in rows {
            let mut tx = self.pool.begin().await?;
            sqlx::query("SELECT id FROM builds WHERE id=$1 FOR UPDATE")
                .bind(row.get::<Uuid, _>("build_id"))
                .execute(&mut *tx)
                .await?;
            // Heartbeat may have renewed the lease while we waited for the build lock.
            let Some(run)=sqlx::query("SELECT r.*,b.cancel_requested FROM planning_runs r JOIN builds b ON b.id=r.build_id LEFT JOIN ai_workers w ON w.id=r.worker_id WHERE r.id=$1 AND r.state='leased' AND (r.lease_expires_at<=clock_timestamp() OR r.deadline_at<=clock_timestamp() OR w.revoked OR w.certificate_expires_at<=clock_timestamp() OR b.cancel_requested) FOR UPDATE OF r").bind(row.get::<Uuid,_>("id")).fetch_optional(&mut *tx).await? else {continue;};
            let expired = run
                .get::<Option<chrono::DateTime<chrono::Utc>>, _>("deadline_at")
                .is_none_or(|time| time <= chrono::Utc::now());
            let canceled: bool = run.get("cancel_requested");
            if expired || canceled {
                sqlx::query("UPDATE planning_runs SET state=$2,error='planning deadline reached',finished_at=now() WHERE id=$1").bind(run.get::<Uuid,_>("id")).bind(if canceled{"canceled"}else{"failed"}).execute(&mut *tx).await?;
                sqlx::query("UPDATE builds SET status=$2,error=CASE WHEN $2='failed' THEN 'planning deadline reached' ELSE error END,finished_at=now() WHERE id=$1").bind(run.get::<Uuid,_>("build_id")).bind(if canceled{"canceled"}else{"failed"}).execute(&mut *tx).await?;
            } else {
                sqlx::query("UPDATE planning_runs SET state='queued',worker_id=NULL,lease_hash=NULL,lease_expires_at=NULL WHERE id=$1").bind(run.get::<Uuid,_>("id")).execute(&mut *tx).await?;
            }
            tx.commit().await?;
        }
        Ok(())
    }
}
