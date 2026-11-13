//! Planning leases, budgets, revisions and their atomic state transitions.
use super::ControlStore;
use crate::{crypto, execution_plan::ExecutionPlan, pki};
use anyhow::{Result, ensure};
use artifactory_database::{params, sql};
use serde_json::{Value, json};
use toasty::stmt::Type;

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
    tx: &mut toasty::db::Transaction<'_>,
    worker: Uuid,
    id: &str,
    token: &str,
) -> std::result::Result<sql::Record, Status> {
    let id = Uuid::parse_str(id).map_err(|_| Status::invalid_argument("invalid planning run"))?;
    let build: Uuid = sql::optional_scalar(
        tx,
        "SELECT build_id FROM planning_runs WHERE id=$1",
        params![id],
    )
    .await
    .map_err(internal)?
    .ok_or_else(|| Status::not_found("planning run not found"))?;
    sql::execute(
        tx,
        "SELECT id FROM builds WHERE id=$1 FOR UPDATE",
        params![build],
    )
    .await
    .map_err(internal)?;
    sql::execute(
        tx,
        "SELECT id FROM planning_runs WHERE id=$1 FOR UPDATE",
        params![id],
    )
    .await
    .map_err(internal)?;
    sql::execute(
        tx,
        "SELECT id FROM ai_workers WHERE id=$1 FOR SHARE",
        params![worker],
    )
    .await
    .map_err(internal)?;
    sql::optional_record(tx, "SELECT r.*,b.project_id,b.source_snapshot_id,b.goal,b.target_requirements,b.planning_context,b.parameters,b.selected_targets,b.current_plan_id,b.revision_count,b.release_version FROM planning_runs r JOIN builds b ON b.id=r.build_id JOIN ai_workers w ON w.id=r.worker_id WHERE r.id=$1 AND r.worker_id=$2 AND r.lease_hash=$3 AND r.state='leased' AND r.lease_expires_at>clock_timestamp() AND r.deadline_at>clock_timestamp() AND NOT b.cancel_requested AND b.status IN ('planning','replanning') AND NOT w.revoked AND w.certificate_expires_at>clock_timestamp()", params![id, worker, crypto::hash(token)], &[("id", Type::Uuid),("build_id", Type::Uuid),("worker_id", Type::Uuid),("purpose", Type::String),("state", Type::String),("failed_targets", Type::List(Box::new(Type::String))),("model_config", Type::String),("lease_hash", Type::String),("lease_expires_at", Type::Timestamp),("deadline_at", Type::Timestamp),("calls", Type::I32),("usage", Type::String),("error", Type::String),("created_at", Type::Timestamp),("finished_at", Type::Timestamp),("authority", Type::String),("project_id", Type::Uuid),("source_snapshot_id", Type::Uuid),("goal", Type::String),("target_requirements", Type::String),("planning_context", Type::String),("parameters", Type::String),("selected_targets", Type::List(Box::new(Type::String))),("current_plan_id", Type::Uuid),("revision_count", Type::I32),("release_version", Type::String)]).await.map_err(internal)?.ok_or_else(||Status::failed_precondition("planning lease expired, canceled or superseded"))
}

pub(super) async fn submission_context(
    tx: &mut toasty::db::Transaction<'_>,
    row: &sql::Record,
) -> Result<SubmissionContext> {
    let current_plan_id: Option<Uuid> = row.decode("current_plan_id")?;
    let previous = if let Some(id) = current_plan_id {
        Some(
            sql::scalar(
                tx,
                "SELECT contents FROM plan_revisions WHERE id=$1",
                params![id],
            )
            .await?,
        )
    } else {
        None
    };
    Ok(SubmissionContext {
        snapshot_id: row.decode("source_snapshot_id")?,
        parameters: row.decode("parameters")?,
        target_requirements: row.decode("target_requirements")?,
        planning_context: row.decode("planning_context")?,
        selected_targets: row.decode("selected_targets")?,
        current_plan_id,
        failed_targets: row.decode("failed_targets")?,
        previous,
    })
}
pub(super) async fn persist_plan(
    tx: &mut toasty::db::Transaction<'_>,
    row: &sql::Record,
    plan: ExecutionPlan,
) -> Result<Value> {
    let build: Uuid = row.decode("build_id")?;
    let snapshot: Uuid = row.decode("source_snapshot_id")?;
    let current: Option<Uuid> = row.decode("current_plan_id")?;
    let failed: Vec<String> = row.decode("failed_targets")?;
    let plan_id = Uuid::new_v4();
    let contents = serde_json::to_value(&plan)?;
    super::models::PlanRevision::create()
        .id(plan_id)
        .build_id(build)
        .run_id(row.decode::<Uuid>("id")?)
        .revision(row.decode::<i32>("revision_count")?)
        .source_snapshot_id(snapshot)
        .contents(toasty::Json(contents.clone()))
        .sha256(crypto::hash(serde_json::to_vec(&contents)?))
        .exec(tx)
        .await?;
    for (name, target) in &plan.recipe.targets {
        if current.is_none() {
            super::models::Job::create()
                .id(Uuid::new_v4())
                .build_id(build)
                .target(name)
                .target_config(toasty::Json((serde_json::to_value(target)?).clone()))
                .plan_revision_id(plan_id)
                .exec(tx)
                .await?;
        } else if failed.contains(name) {
            let changed=sql::execute(tx, "UPDATE jobs SET target_config=$3,plan_revision_id=$4,status='queued' WHERE build_id=$1 AND target=$2 AND status IN ('failed','lost')", params![build, name, serde_json::to_value(target)?, plan_id]).await?;
            ensure!(changed == 1, "target is no longer eligible for revision");
        }
    }
    sql::execute(tx, "UPDATE builds SET current_plan_id=$2,config_snapshot=$3,status='queued',error=NULL,finished_at=NULL WHERE id=$1", params![build, plan_id, serde_json::to_value(&plan.recipe)?]).await?;
    sql::execute(
        tx,
        "UPDATE planning_runs SET state='succeeded',finished_at=now() WHERE id=$1",
        params![row.decode::<Uuid>("id")?],
    )
    .await?;
    Ok(json!({"accepted":true,"plan_revision_id":plan_id}))
}
impl ControlStore {
    pub(crate) async fn planner_identity(&self, hash: String) -> std::result::Result<Uuid, Status> {
        sql::optional_scalar(&mut self.pool.clone(), "SELECT id FROM ai_workers WHERE certificate_hash=$1 AND NOT revoked AND certificate_expires_at>clock_timestamp()", params![hash]).await.map_err(internal)?.ok_or_else(||Status::unauthenticated("Agent worker revoked, expired or unknown"))
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
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await.map_err(internal)?;
        let token:Option<Uuid>=sql::optional_scalar(&mut tx, "SELECT id FROM ai_enrollment_tokens WHERE token_hash=$1 AND used_at IS NULL AND expires_at>clock_timestamp() FOR UPDATE", params![crypto::hash(token_value)]).await.map_err(internal)?;
        let token = token.ok_or_else(|| Status::unauthenticated("invalid enrollment token"))?;
        let id = Uuid::new_v4();
        let certificate = issue(id).await?;
        sql::execute(&mut tx, "INSERT INTO ai_workers(id,name,certificate_hash,certificate_expires_at) VALUES($1,$2,$3,now()+interval '29 days')", params![id, name, pki::certificate_hash(&certificate).map_err(internal)?]).await.map_err(internal)?;
        sql::execute(
            &mut tx,
            "UPDATE ai_enrollment_tokens SET used_at=now() WHERE id=$1",
            params![token],
        )
        .await
        .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(super::executions::EnrolledNode { id, certificate })
    }
    pub(crate) async fn planning_nodes(&self, build: Uuid) -> Result<Vec<Value>> {
        let context: Value = sql::scalar(
            &mut self.pool.clone(),
            "SELECT planning_context FROM builds WHERE id=$1",
            params![build],
        )
        .await?;
        let pools: Vec<Uuid> =
            serde_json::from_value(context.get("pool_ids").cloned().unwrap_or(json!([])))?;
        Ok(sql::scalars(&mut self.pool.clone(), "SELECT jsonb_build_object('name',n.name,'os',os,'arch',arch,'labels',labels,'capabilities',capabilities,'signing_profiles',signing_profiles,'tools',diagnostics->'tools','online',last_seen>now()-interval '30 seconds') FROM nodes n WHERE NOT revoked AND pool_id=ANY($1)", params![pools]).await?)
    }
    pub(crate) async fn claim_planning(
        &self,
        worker: Uuid,
    ) -> std::result::Result<Option<PlanningClaim>, Status> {
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await.map_err(internal)?;
        let draining: bool = sql::scalar(
            &mut tx,
            "UPDATE ai_workers SET last_seen=now() WHERE id=$1 RETURNING draining",
            params![worker],
        )
        .await
        .map_err(internal)?;
        let active: bool = sql::scalar(
            &mut tx,
            "SELECT EXISTS(SELECT 1 FROM planning_runs WHERE worker_id=$1 AND state='leased')",
            params![worker],
        )
        .await
        .map_err(internal)?;
        if draining || active {
            tx.commit().await.map_err(internal)?;
            return Ok(None);
        }
        let Some(row)=sql::optional_record(&mut tx, "SELECT r.id,r.build_id,r.model_config,r.calls,r.usage FROM planning_runs r JOIN builds b ON b.id=r.build_id WHERE r.state='queued' AND r.authority='poll' AND NOT b.cancel_requested AND b.status IN ('planning','replanning') ORDER BY r.created_at FOR UPDATE OF r SKIP LOCKED LIMIT 1", params![], &[("id", Type::Uuid),("build_id", Type::Uuid),("model_config", Type::String),("calls", Type::I32),("usage", Type::String)]).await.map_err(internal)? else { tx.commit().await.map_err(internal)?; return Ok(None); };
        let id: Uuid = row.decode("id").map_err(internal)?;
        let build: Uuid = row.decode("build_id").map_err(internal)?;
        let config: Value = row.decode("model_config").map_err(internal)?;
        let timeout = config["timeout_seconds"].as_i64().unwrap_or(600);
        let token = crypto::token();
        let remaining:i64=sql::scalar(&mut tx, "UPDATE planning_runs SET worker_id=$2,state='leased',lease_hash=$3,deadline_at=coalesce(deadline_at,clock_timestamp()+make_interval(secs=>$4)),lease_expires_at=clock_timestamp()+interval '90 seconds' WHERE id=$1 RETURNING greatest(1,extract(epoch FROM deadline_at-clock_timestamp())::bigint)", params![id, worker, crypto::hash(&token), timeout as f64]).await.map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        let b=sql::record(&mut self.pool.clone(), "SELECT goal,requested_ref,commit_sha,parameters,target_requirements,planning_context,source_snapshot_id,revision_count,clarification FROM builds WHERE id=$1", params![build], &[("goal", Type::String),("requested_ref", Type::String),("commit_sha", Type::String),("parameters", Type::String),("target_requirements", Type::String),("planning_context", Type::String),("source_snapshot_id", Type::Uuid),("revision_count", Type::I32),("clarification", Type::String)]).await.map_err(internal)?;
        let current:Option<Value>=sql::optional_scalar(&mut self.pool.clone(), "SELECT contents FROM plan_revisions WHERE id=(SELECT current_plan_id FROM builds WHERE id=$1)", params![build]).await.map_err(internal)?;
        let failed:Vec<Value>=sql::scalars(&mut self.pool.clone(), "SELECT jsonb_build_object('target',j.target,'error',a.error,'attempt_id',a.id) FROM jobs j JOIN LATERAL (SELECT id,error FROM attempts WHERE job_id=j.id ORDER BY created_at DESC LIMIT 1) a ON true WHERE j.build_id=$1 AND j.status IN ('failed','lost')", params![build]).await.map_err(internal)?;
        let checkpoint:Option<Value>=sql::optional_scalar(&mut self.pool.clone(), "SELECT payload FROM planning_events WHERE run_id=$1 AND kind='checkpoint' ORDER BY id DESC LIMIT 1", params![id]).await.map_err(internal)?;
        let context = json!({"build_id":build,"goal":b.decode::<Option<String>>("goal").map_err(internal)?,"ref":b.decode::<String>("requested_ref").map_err(internal)?,"commit":b.decode::<Option<String>>("commit_sha").map_err(internal)?,"parameters":b.decode::<Value>("parameters").map_err(internal)?,"target_requirements":b.decode::<Value>("target_requirements").map_err(internal)?,"project_context":b.decode::<Value>("planning_context").map_err(internal)?,"snapshot_id":b.decode::<Uuid>("source_snapshot_id").map_err(internal)?,"revision":b.decode::<i32>("revision_count").map_err(internal)?,"current_plan":current,"failed_targets":failed,"nodes":self.planning_nodes(build).await.map_err(internal)?,"checkpoint":checkpoint,"usage":row.decode::<Value>("usage").map_err(internal)?});
        Ok(Some(PlanningClaim {
            id,
            token,
            config: config.clone(),
            context,
            timeout_seconds: remaining.min(timeout) as u32,
            max_calls: (config["max_calls"].as_i64().unwrap_or(20)
                - row.decode::<i32>("calls").map_err(internal)? as i64)
                .max(0) as u32,
        }))
    }
    pub(crate) async fn renew_planning(
        &self,
        worker: Uuid,
        run_id: &str,
        token: &str,
    ) -> std::result::Result<bool, Status> {
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await.map_err(internal)?;
        if lock_run(&mut tx, worker, run_id, token).await.is_err() {
            return Ok(true);
        }
        sql::execute(&mut tx, "UPDATE planning_runs SET lease_expires_at=least(deadline_at,clock_timestamp()+interval '90 seconds') WHERE id=$1", params![Uuid::parse_str(run_id).unwrap()]).await.map_err(internal)?;
        sql::execute(
            &mut tx,
            "UPDATE ai_workers SET last_seen=now() WHERE id=$1",
            params![worker],
        )
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
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await.map_err(internal)?;
        let row = lock_run(&mut tx, worker, run_id, token).await?;
        let build: Uuid = row.decode("build_id").map_err(internal)?;
        let run: Uuid = row.decode("id").map_err(internal)?;
        let result: Result<Value> = match name {
            "submit_plan" => {
                // A rejected plan must leave no jobs or revision behind, including
                // failures after the first SQL write. Keep the lease transaction.
                let mut savepoint = tx.transaction().await.map_err(internal)?;
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
                    sql::execute(
                        &mut tx,
                        "UPDATE builds SET status='needs_input',clarification=$2 WHERE id=$1",
                        params![build, json!({"question":question})],
                    )
                    .await
                    .map_err(internal)?;
                    sql::execute(&mut tx, "UPDATE planning_runs SET state='needs_input',finished_at=now() WHERE id=$1", params![run]).await.map_err(internal)?;
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
                let snapshot = row.decode("source_snapshot_id").map_err(internal)?;
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
        super::models::PlanningEvent::create()
            .run_id(run)
            .kind("tool")
            .payload(toasty::Json(
                (json!({"name":name,"accepted":value.get("accepted"),"error":value.get("error")}))
                    .clone(),
            ))
            .exec(&mut tx)
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
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await.map_err(internal)?;
        let row = lock_run(&mut tx, worker, run_id, token).await?;
        let run: Uuid = row.decode("id").map_err(internal)?;
        let payload: Value = serde_json::from_str(payload_json)
            .map_err(|_| Status::invalid_argument("invalid event JSON"))?;
        let mut wait_ms: i64 = 0;
        if kind == "model_call" {
            let config = row.decode::<Value>("model_config").map_err(internal)?;
            let limit = config["max_calls"].as_i64().unwrap_or(20);
            if row.decode::<i32>("calls").map_err(internal)? as i64 >= limit {
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
                wait_ms = sql::optional_scalar(&mut tx, "UPDATE model_profiles SET next_call_at=greatest(coalesce(next_call_at,clock_timestamp()),clock_timestamp())+make_interval(secs=>$2) WHERE id=$1 RETURNING greatest(0,ceil(extract(epoch FROM next_call_at-clock_timestamp())*1000-$2*1000)::bigint)", params![profile, interval]).await.map_err(internal)?.unwrap_or(0);
            }
            sql::execute(
                &mut tx,
                "UPDATE planning_runs SET calls=calls+1 WHERE id=$1",
                params![run],
            )
            .await
            .map_err(internal)?;
        } else if kind == "usage" {
            sql::execute(
                &mut tx,
                "UPDATE planning_runs SET usage=$2 WHERE id=$1",
                params![run, &payload],
            )
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
            sql::execute(
                &mut tx,
                "UPDATE planning_runs SET state='failed',error=$2,finished_at=now() WHERE id=$1",
                params![run, &message],
            )
            .await
            .map_err(internal)?;
            sql::execute(
                &mut tx,
                "UPDATE builds SET status='failed',error=$2,finished_at=now() WHERE id=$1",
                params![row.decode::<Uuid>("build_id").map_err(internal)?, message],
            )
            .await
            .map_err(internal)?;
        }
        super::models::PlanningEvent::create()
            .run_id(run)
            .kind(kind)
            .payload(toasty::Json((payload).clone()))
            .exec(&mut tx)
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
        let rows=sql::records(&mut self.pool.clone(), "SELECT l.object_key,l.step FROM log_chunks l JOIN attempts a ON a.id=l.attempt_id JOIN jobs j ON j.id=a.job_id WHERE j.build_id=$1 AND ($2='' OR j.target=$2) ORDER BY l.id DESC LIMIT 24", params![build, target], &[("object_key", Type::String),("step", Type::String)]).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                Ok::<_, artifactory_database::Error>(PlanningLog {
                    object_key: row.decode("object_key")?,
                    step: row.decode("step")?,
                })
            })
            .collect::<artifactory_database::Result<_>>()?)
    }
    pub(crate) async fn reap_planning(&self) -> Result<()> {
        let rows=sql::records(&mut self.pool.clone(), "SELECT r.id,r.build_id FROM planning_runs r LEFT JOIN ai_workers w ON w.id=r.worker_id WHERE r.authority='poll' AND r.state='leased' AND (r.lease_expires_at<=clock_timestamp() OR r.deadline_at<=clock_timestamp() OR w.revoked OR w.certificate_expires_at<=clock_timestamp())", params![], &[("id", Type::Uuid),("build_id", Type::Uuid)]).await?;
        for row in rows {
            let mut tx_db = self.pool.clone();
            let mut tx = tx_db.transaction().await?;
            sql::execute(
                &mut tx,
                "SELECT id FROM builds WHERE id=$1 FOR UPDATE",
                params![row.decode::<Uuid>("build_id")?],
            )
            .await?;
            // Heartbeat may have renewed the lease while we waited for the build lock.
            let Some(run)=sql::optional_record(&mut tx, "SELECT r.*,b.cancel_requested FROM planning_runs r JOIN builds b ON b.id=r.build_id LEFT JOIN ai_workers w ON w.id=r.worker_id WHERE r.id=$1 AND r.state='leased' AND (r.lease_expires_at<=clock_timestamp() OR r.deadline_at<=clock_timestamp() OR w.revoked OR w.certificate_expires_at<=clock_timestamp() OR b.cancel_requested) FOR UPDATE OF r", params![row.decode::<Uuid>("id")?], &[("id", Type::Uuid),("build_id", Type::Uuid),("worker_id", Type::Uuid),("purpose", Type::String),("state", Type::String),("failed_targets", Type::List(Box::new(Type::String))),("model_config", Type::String),("lease_hash", Type::String),("lease_expires_at", Type::Timestamp),("deadline_at", Type::Timestamp),("calls", Type::I32),("usage", Type::String),("error", Type::String),("created_at", Type::Timestamp),("finished_at", Type::Timestamp),("authority", Type::String),("cancel_requested", Type::Bool)]).await? else {continue;};
            let expired = run
                .decode::<Option<chrono::DateTime<chrono::Utc>>>("deadline_at")?
                .is_none_or(|time| time <= chrono::Utc::now());
            let canceled: bool = run.decode("cancel_requested")?;
            if expired || canceled {
                sql::execute(&mut tx, "UPDATE planning_runs SET state=$2,error='planning deadline reached',finished_at=now() WHERE id=$1", params![run.decode::<Uuid>("id")?, if canceled{"canceled"}else{"failed"}]).await?;
                sql::execute(&mut tx, "UPDATE builds SET status=$2,error=CASE WHEN $2='failed' THEN 'planning deadline reached' ELSE error END,finished_at=now() WHERE id=$1", params![run.decode::<Uuid>("build_id")?, if canceled{"canceled"}else{"failed"}]).await?;
            } else {
                sql::execute(&mut tx, "UPDATE planning_runs SET state='queued',worker_id=NULL,lease_hash=NULL,lease_expires_at=NULL WHERE id=$1", params![run.decode::<Uuid>("id")?]).await?;
            }
            tx.commit().await?;
        }
        Ok(())
    }
}
