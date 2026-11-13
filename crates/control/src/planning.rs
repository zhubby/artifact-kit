use crate::{
    crypto, execution_plan::ExecutionPlan, pki, protocol::*, recipe::Recipe, source_cache,
    state::AppState,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sqlx::{Acquire, Row, postgres::PgRow};
use tonic::{
    Request, Response, Status,
    transport::server::{TcpConnectInfo, TlsConnectInfo},
};
use uuid::Uuid;

#[derive(Clone)]
pub struct PlanningService(pub AppState);
fn internal(error: impl std::fmt::Display) -> Status {
    tracing::error!(%error,"planning operation failed");
    Status::internal("planning operation failed")
}
async fn identity<T>(state: &AppState, request: &Request<T>) -> std::result::Result<Uuid, Status> {
    let certificate = request
        .extensions()
        .get::<TlsConnectInfo<TcpConnectInfo>>()
        .and_then(|info| info.peer_certs())
        .and_then(|certs| certs.first().cloned())
        .ok_or_else(|| Status::unauthenticated("AI worker certificate required"))?;
    sqlx::query_scalar("SELECT id FROM ai_workers WHERE certificate_hash=$1 AND NOT revoked AND certificate_expires_at>clock_timestamp()")
        .bind(crypto::hash(certificate.as_ref())).fetch_optional(&state.pool).await.map_err(internal)?.ok_or_else(||Status::unauthenticated("AI worker revoked, expired or unknown"))
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

#[tonic::async_trait]
impl planner_enrollment_server::PlannerEnrollment for PlanningService {
    async fn enroll(
        &self,
        request: Request<PlannerEnrollRequest>,
    ) -> std::result::Result<Response<PlannerEnrollReply>, Status> {
        let input = request.into_inner();
        if input.name.trim().is_empty() || input.name.len() > 100 || input.token.len() > 256 {
            return Err(Status::invalid_argument("invalid worker enrollment"));
        }
        let state = &self.0;
        let mut tx = state.pool.begin().await.map_err(internal)?;
        let token:Option<Uuid>=sqlx::query_scalar("SELECT id FROM ai_enrollment_tokens WHERE token_hash=$1 AND used_at IS NULL AND expires_at>clock_timestamp() FOR UPDATE").bind(crypto::hash(&input.token)).fetch_optional(&mut *tx).await.map_err(internal)?;
        let token = token.ok_or_else(|| Status::unauthenticated("invalid enrollment token"))?;
        let id = Uuid::new_v4();
        let certificate = pki::sign(&state.settings, id, &input.csr_pem)
            .await
            .map_err(internal)?;
        sqlx::query("INSERT INTO ai_workers(id,name,certificate_hash,certificate_expires_at) VALUES($1,$2,$3,now()+interval '29 days')").bind(id).bind(input.name).bind(pki::certificate_hash(&certificate).map_err(internal)?).execute(&mut *tx).await.map_err(internal)?;
        sqlx::query("UPDATE ai_enrollment_tokens SET used_at=now() WHERE id=$1")
            .bind(token)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(Response::new(PlannerEnrollReply {
            worker_id: id.to_string(),
            certificate_pem: certificate,
            ca_pem: tokio::fs::read_to_string(state.settings.pki_dir.join("ca.pem"))
                .await
                .map_err(internal)?,
            heartbeat_seconds: 10,
            lease_seconds: 90,
        }))
    }
}

pub async fn nodes(state: &AppState, build: Uuid) -> Result<Vec<Value>> {
    let context: Value = sqlx::query_scalar("SELECT planning_context FROM builds WHERE id=$1")
        .bind(build)
        .fetch_one(&state.pool)
        .await?;
    let pools: Vec<Uuid> =
        serde_json::from_value(context.get("pool_ids").cloned().unwrap_or(json!([])))?;
    Ok(sqlx::query_scalar("SELECT jsonb_build_object('name',n.name,'os',os,'arch',arch,'labels',labels,'capabilities',capabilities,'signing_profiles',signing_profiles,'tools',diagnostics->'tools','online',last_seen>now()-interval '30 seconds') FROM nodes n WHERE NOT revoked AND pool_id=ANY($1)")
        .bind(pools).fetch_all(&state.pool).await?)
}

#[tonic::async_trait]
impl planning_server::Planning for PlanningService {
    async fn poll(
        &self,
        request: Request<PlannerPollRequest>,
    ) -> std::result::Result<Response<PlannerTask>, Status> {
        let state = &self.0;
        let worker = identity(state, &request).await?;
        let mut tx = state.pool.begin().await.map_err(internal)?;
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
            return Ok(Response::new(PlannerTask::default()));
        }
        let Some(row)=sqlx::query("SELECT r.id,r.build_id,r.model_config,r.calls,r.usage FROM planning_runs r JOIN builds b ON b.id=r.build_id WHERE r.state='queued' AND r.authority='poll' AND NOT b.cancel_requested AND b.status IN ('planning','replanning') ORDER BY r.created_at FOR UPDATE OF r SKIP LOCKED LIMIT 1").fetch_optional(&mut *tx).await.map_err(internal)? else { tx.commit().await.map_err(internal)?; return Ok(Response::new(PlannerTask::default())); };
        let id: Uuid = row.get("id");
        let build: Uuid = row.get("build_id");
        let config: Value = row.get("model_config");
        let timeout = config["timeout_seconds"].as_i64().unwrap_or(600);
        let token = crypto::token();
        let remaining:i64=sqlx::query_scalar("UPDATE planning_runs SET worker_id=$2,state='leased',lease_hash=$3,deadline_at=coalesce(deadline_at,clock_timestamp()+make_interval(secs=>$4)),lease_expires_at=clock_timestamp()+interval '90 seconds' WHERE id=$1 RETURNING greatest(1,extract(epoch FROM deadline_at-clock_timestamp())::bigint)")
            .bind(id).bind(worker).bind(crypto::hash(&token)).bind(timeout as f64).fetch_one(&mut *tx).await.map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        let b=sqlx::query("SELECT goal,requested_ref,commit_sha,parameters,target_requirements,planning_context,source_snapshot_id,revision_count,clarification FROM builds WHERE id=$1").bind(build).fetch_one(&state.pool).await.map_err(internal)?;
        let current:Option<Value>=sqlx::query_scalar("SELECT contents FROM plan_revisions WHERE id=(SELECT current_plan_id FROM builds WHERE id=$1)").bind(build).fetch_optional(&state.pool).await.map_err(internal)?;
        let failed:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('target',j.target,'error',a.error,'attempt_id',a.id) FROM jobs j JOIN LATERAL (SELECT id,error FROM attempts WHERE job_id=j.id ORDER BY created_at DESC LIMIT 1) a ON true WHERE j.build_id=$1 AND j.status IN ('failed','lost')").bind(build).fetch_all(&state.pool).await.map_err(internal)?;
        let checkpoint:Option<Value>=sqlx::query_scalar("SELECT payload FROM planning_events WHERE run_id=$1 AND kind='checkpoint' ORDER BY id DESC LIMIT 1").bind(id).fetch_optional(&state.pool).await.map_err(internal)?;
        let context = json!({"build_id":build,"goal":b.get::<Option<String>,_>("goal"),"ref":b.get::<String,_>("requested_ref"),"commit":b.get::<Option<String>,_>("commit_sha"),"parameters":b.get::<Value,_>("parameters"),"target_requirements":b.get::<Value,_>("target_requirements"),"project_context":b.get::<Value,_>("planning_context"),"snapshot_id":b.get::<Uuid,_>("source_snapshot_id"),"revision":b.get::<i32,_>("revision_count"),"current_plan":current,"failed_targets":failed,"nodes":nodes(state,build).await.map_err(internal)?,"checkpoint":checkpoint,"usage":row.get::<Value,_>("usage")});
        let mut public_config = config.clone();
        let key = crypto::open(
            &state.settings.master_key,
            config["key_ciphertext"]
                .as_str()
                .ok_or_else(|| Status::internal("model key unavailable"))?,
        )
        .map_err(internal)?;
        public_config
            .as_object_mut()
            .unwrap()
            .remove("key_ciphertext");
        public_config["api_key"] = json!(String::from_utf8(key).map_err(internal)?);
        Ok(Response::new(PlannerTask {
            run_id: id.to_string(),
            lease_token: token,
            context_json: context.to_string(),
            model_config_json: public_config.to_string(),
            timeout_seconds: remaining.min(timeout) as u32,
            max_calls: (config["max_calls"].as_i64().unwrap_or(20)
                - row.get::<i32, _>("calls") as i64)
                .max(0) as u32,
        }))
    }
    async fn heartbeat(
        &self,
        request: Request<PlannerLeaseRequest>,
    ) -> std::result::Result<Response<PlannerHeartbeat>, Status> {
        let state = &self.0;
        let worker = identity(state, &request).await?;
        let input = request.into_inner();
        let mut tx = state.pool.begin().await.map_err(internal)?;
        if lock_run(&mut tx, worker, &input.run_id, &input.lease_token)
            .await
            .is_err()
        {
            return Ok(Response::new(PlannerHeartbeat { canceled: true }));
        }
        sqlx::query("UPDATE planning_runs SET lease_expires_at=least(deadline_at,clock_timestamp()+interval '90 seconds') WHERE id=$1").bind(Uuid::parse_str(&input.run_id).unwrap()).execute(&mut *tx).await.map_err(internal)?;
        sqlx::query("UPDATE ai_workers SET last_seen=now() WHERE id=$1")
            .bind(worker)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(Response::new(PlannerHeartbeat { canceled: false }))
    }
    async fn tool(
        &self,
        request: Request<PlannerToolRequest>,
    ) -> std::result::Result<Response<PlannerToolReply>, Status> {
        let state = &self.0;
        let worker = identity(state, &request).await?;
        let input = request.into_inner();
        if input.arguments_json.len() > 384 * 1024 {
            return Err(Status::invalid_argument("tool input exceeds limit"));
        }
        let args: Value = serde_json::from_str(&input.arguments_json)
            .map_err(|_| Status::invalid_argument("invalid tool arguments"))?;
        let mut tx = state.pool.begin().await.map_err(internal)?;
        let row = lock_run(&mut tx, worker, &input.run_id, &input.lease_token).await?;
        let build: Uuid = row.get("build_id");
        let run: Uuid = row.get("id");
        let result: Result<Value> = match input.name.as_str() {
            "submit_plan" => {
                // A rejected plan must leave no jobs or revision behind, including
                // failures after the first SQL write. Keep the lease transaction.
                let mut savepoint = tx.begin().await.map_err(internal)?;
                let result = submit(
                    state,
                    &mut savepoint,
                    &row,
                    args.get("plan").cloned().unwrap_or(args.clone()),
                    false,
                )
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
            "node_capabilities" => nodes(state, build)
                .await
                .map(|value| json!({"nodes":value})),
            "read_logs" => read_logs(state, build, &args).await,
            "list_tree" | "read_file" | "search_source" | "git_metadata" => {
                let snapshot = row.get("source_snapshot_id");
                // Source tools are read-only; do not hold the cancellation lock during I/O.
                tx.commit().await.map_err(internal)?;
                let result = source_cache::inspect_tool(state, snapshot, &input.name, &args).await;
                return Ok(Response::new(PlannerToolReply {
                    result_json: match result {
                        Ok(v) => v,
                        Err(e) => json!({"error":e.to_string()}),
                    }
                    .to_string(),
                }));
            }
            _ => Ok(json!({"error":"unknown or prohibited tool"})),
        };
        let value = match result {
            Ok(value) => value,
            Err(error) => json!({"error":error.to_string()}),
        };
        sqlx::query("INSERT INTO planning_events(run_id,kind,payload) VALUES($1,'tool',$2)").bind(run).bind(json!({"name":input.name,"accepted":value.get("accepted"),"error":value.get("error")})).execute(&mut *tx).await.map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(Response::new(PlannerToolReply {
            result_json: value.to_string(),
        }))
    }
    async fn report(
        &self,
        request: Request<PlannerReportRequest>,
    ) -> std::result::Result<Response<PlannerToolReply>, Status> {
        let state = &self.0;
        let worker = identity(state, &request).await?;
        let input = request.into_inner();
        if input.payload_json.len() > 512 * 1024
            || !["model_call", "usage", "summary", "checkpoint", "failed"]
                .contains(&input.kind.as_str())
        {
            return Err(Status::invalid_argument("invalid planning event"));
        }
        let mut tx = state.pool.begin().await.map_err(internal)?;
        let row = lock_run(&mut tx, worker, &input.run_id, &input.lease_token).await?;
        let run: Uuid = row.get("id");
        let payload: Value = serde_json::from_str(&input.payload_json)
            .map_err(|_| Status::invalid_argument("invalid event JSON"))?;
        let mut wait_ms: i64 = 0;
        if input.kind == "model_call" {
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
        } else if input.kind == "usage" {
            sqlx::query("UPDATE planning_runs SET usage=$2 WHERE id=$1")
                .bind(run)
                .bind(&payload)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
        } else if input.kind == "failed" {
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
            .bind(input.kind)
            .bind(payload)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(Response::new(PlannerToolReply {
            result_json: json!({"ok":true,"wait_ms":wait_ms}).to_string(),
        }))
    }
}

pub(crate) async fn read_logs(state: &AppState, build: Uuid, args: &Value) -> Result<Value> {
    let target = args.get("target").and_then(Value::as_str).unwrap_or("");
    let rows=sqlx::query("SELECT l.object_key,l.step FROM log_chunks l JOIN attempts a ON a.id=l.attempt_id JOIN jobs j ON j.id=a.job_id WHERE j.build_id=$1 AND ($2='' OR j.target=$2) ORDER BY l.id DESC LIMIT 24")
        .bind(build).bind(target).fetch_all(&state.pool).await?;
    let mut result = Vec::new();
    let mut size = 0;
    for row in rows.into_iter().rev() {
        let bytes = state
            .storage
            .get_log(&row.get::<String, _>("object_key"))
            .await?;
        let text = String::from_utf8_lossy(&bytes);
        let text = text
            .chars()
            .take((128 * 1024usize).saturating_sub(size))
            .collect::<String>();
        size += text.len();
        result.push(json!({"step":row.get::<String,_>("step"),"content":text}));
        if size >= 128 * 1024 {
            break;
        }
    }
    Ok(json!({"logs":result}))
}

fn target_requirements(recipe: &Recipe, requirements: &Value) -> Result<()> {
    let requirements = requirements
        .as_array()
        .context("target requirements must be an array")?;
    for requirement in requirements {
        let os = requirement["os"]
            .as_str()
            .context("target OS is required")?;
        let arch = requirement["arch"]
            .as_str()
            .context("target architecture is required")?;
        ensure!(
            recipe
                .targets
                .values()
                .any(|target| target.os == os && target.arch == arch),
            "plan does not cover requested platform {os}/{arch}"
        );
    }
    if !requirements.is_empty() {
        ensure!(
            recipe.targets.values().all(|t| requirements
                .iter()
                .any(|r| r["os"] == t.os && r["arch"] == t.arch)),
            "plan adds an unrequested platform"
        );
    }
    Ok(())
}

pub(crate) async fn submit(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    row: &PgRow,
    value: Value,
    evidence_verified: bool,
) -> Result<Value> {
    let mut plan = ExecutionPlan::parse(value)?;
    let build: Uuid = row.get("build_id");
    let snapshot: Uuid = row.get("source_snapshot_id");
    let source = source_cache::load(state, snapshot).await?;
    if !evidence_verified {
        source_cache::validate_evidence(
            state,
            &source,
            &plan
                .evidence
                .iter()
                .map(|item| item.path.clone())
                .collect::<Vec<_>>(),
        )
        .await?;
    }
    plan.recipe
        .environment(&row.get::<Value, _>("parameters"))?;
    target_requirements(&plan.recipe, &row.get::<Value, _>("target_requirements"))?;
    let context: Value = row.get("planning_context");
    let permitted: Vec<String> = serde_json::from_value(
        context
            .get("signing_profiles")
            .cloned()
            .unwrap_or(json!([])),
    )?;
    let selected: Vec<String> = row.get("selected_targets");
    ensure!(
        selected.is_empty()
            || plan.recipe.targets.keys().all(|key| selected.contains(key))
                && selected
                    .iter()
                    .all(|key| plan.recipe.targets.contains_key(key)),
        "plan target names differ from requested targets"
    );
    for target in plan.recipe.targets.values() {
        ensure!(
            target
                .signing_profiles
                .iter()
                .all(|name| permitted.contains(name)),
            "plan exceeds project signing authorization"
        );
    }
    let current: Option<Uuid> = row.get("current_plan_id");
    let failed: Vec<String> = row.get("failed_targets");
    if let Some(id) = current {
        let previous: Value = sqlx::query_scalar("SELECT contents FROM plan_revisions WHERE id=$1")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
        let previous = ExecutionPlan::parse(previous)?;
        ensure!(
            plan.recipe
                .targets
                .keys()
                .eq(previous.recipe.targets.keys()),
            "revision cannot change target set"
        );
        for (name, target) in &plan.recipe.targets {
            let old = &previous.recipe.targets[name];
            ensure!(
                target.os == old.os
                    && target.arch == old.arch
                    && target
                        .signing_profiles
                        .iter()
                        .all(|p| old.signing_profiles.contains(p)),
                "revision cannot change platform or expand signing authorization"
            );
            if !failed.contains(name) {
                ensure!(
                    serde_json::to_value(target)? == serde_json::to_value(old)?,
                    "revision cannot change a successful target"
                );
            }
        }
    }
    // Publication is controlled by explicit API requests and release authorization.
    plan.recipe.release.enabled = true;
    if source.repositories.iter().any(|repo| !repo.lfs.is_empty()) {
        for target in plan.recipe.targets.values_mut() {
            if !target.capabilities.iter().any(|c| c == "git-lfs") {
                target.capabilities.push("git-lfs".into());
            }
        }
    }
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

pub async fn reap(state: &AppState) -> Result<()> {
    let rows=sqlx::query("SELECT r.id,r.build_id FROM planning_runs r LEFT JOIN ai_workers w ON w.id=r.worker_id WHERE r.authority='poll' AND r.state='leased' AND (r.lease_expires_at<=clock_timestamp() OR r.deadline_at<=clock_timestamp() OR w.revoked OR w.certificate_expires_at<=clock_timestamp())")
        .fetch_all(&state.pool).await?;
    for row in rows {
        let mut tx = state.pool.begin().await?;
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
