//! Durable runtime receipts, outbox delivery and remote source state.
use super::ControlStore;
use anyhow::{Context, Result};
use artifactory_database::{params, sql};
use serde_json::{Value, json};
use toasty::stmt::Type;

use uuid::Uuid;
fn id(v: &Value, key: &str) -> Result<Uuid> {
    Ok(Uuid::parse_str(
        v[key].as_str().context("missing identifier")?,
    )?)
}
pub(crate) struct RuntimeReceipt {
    pub input_hash: String,
    pub result: Value,
}
pub(crate) struct RuntimeRunSource {
    pub build_id: Uuid,
    pub snapshot_id: Uuid,
}
pub(crate) struct RuntimeDelivery {
    pub id: i64,
    pub run_id: Uuid,
    pub kind: String,
    pub version: i64,
    pub ciphertext: Option<String>,
}
pub(crate) struct RuntimeIntentContext {
    pub build_id: Uuid,
    pub project_id: Uuid,
    pub snapshot_id: Uuid,
    pub digest: String,
    pub model: Value,
    pub context: Value,
}
pub(crate) struct RemoteCaptureInput {
    pub project_id: Uuid,
    pub planning_context: Value,
    pub credential_id: Option<Uuid>,
    pub repository_url: String,
    pub requested_ref: String,
    pub model_config_snapshot: Option<Value>,
    pub origin: String,
    pub ciphertext: Option<String>,
}
pub(crate) struct RuntimeEventCommit<'a> {
    pub event: Uuid,
    pub run: Uuid,
    pub build: Uuid,
    pub hash: String,
    pub kind: &'a str,
    pub payload: &'a Value,
    pub validation: Option<String>,
}
pub(crate) struct RemoteSnapshot<'a> {
    pub build: Uuid,
    pub project: Uuid,
    pub snapshot: &'a crate::source_cache::Snapshot,
    pub root_repository: Uuid,
    pub result: &'a Value,
    pub origin: String,
    pub public_origin: String,
    pub model: Option<Value>,
}
impl ControlStore {
    pub(crate) async fn source_project_origin(&self, project: Uuid) -> Result<Option<String>> {
        let origin: Option<String> = sql::optional_scalar(
            &mut self.pool.clone(),
            "SELECT origin FROM source_project_routes WHERE project_id=$1",
            params![project],
        )
        .await?;
        Ok(origin)
    }

    pub(crate) async fn source_origin(&self, snapshot: Uuid) -> Result<Option<String>> {
        Ok(sql::scalar(
            &mut self.pool.clone(),
            "SELECT service_origin FROM source_snapshots WHERE id=$1",
            params![snapshot],
        )
        .await?)
    }

    pub(crate) async fn source_download_origin(&self, snapshot: Uuid) -> Result<Option<String>> {
        Ok(sql::scalar(
            &mut self.pool.clone(),
            "SELECT coalesce(public_origin,service_origin) FROM source_snapshots WHERE id=$1",
            params![snapshot],
        )
        .await?)
    }

    pub(crate) async fn authorize_source_download(
        &self,
        token_hash: String,
        snapshot: Uuid,
    ) -> Result<bool> {
        let valid:bool=sql::scalar(&mut self.pool.clone(), "SELECT EXISTS(SELECT 1 FROM source_tokens t JOIN attempts a ON a.id=t.attempt_id JOIN nodes n ON n.id=a.node_id JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id JOIN source_snapshots ss ON ss.id=t.snapshot_id WHERE t.token_hash=$1 AND t.snapshot_id=$2 AND b.source_snapshot_id=ss.id AND b.project_id=ss.project_id AND t.expires_at>clock_timestamp() AND a.status IN ('leased','running') AND a.lease_expires_at>clock_timestamp() AND NOT b.cancel_requested AND NOT n.revoked AND n.certificate_expires_at>clock_timestamp())", params![token_hash, snapshot]).await?;
        Ok(valid)
    }

    pub(crate) async fn runtime_tool_build(&self, run: Uuid) -> Result<Uuid> {
        let build:Uuid=sql::scalar(&mut self.pool.clone(), "SELECT b.id FROM builds b JOIN planning_runs r ON r.build_id=b.id WHERE r.id=$1 AND r.authority='kubernetes' AND NOT b.cancel_requested AND b.status IN ('planning','replanning')", params![run]).await?;
        Ok(build)
    }

    pub(crate) async fn runtime_receipt(&self, event: Uuid) -> Result<Option<RuntimeReceipt>> {
        let row = sql::optional_record(
            &mut self.pool.clone(),
            "SELECT input_hash,result FROM runtime_receipts WHERE event_id=$1",
            params![event],
            &[("input_hash", Type::String), ("result", Type::String)],
        )
        .await?;
        Ok(row
            .map(|row| {
                Ok::<_, artifactory_database::Error>(RuntimeReceipt {
                    input_hash: row.decode("input_hash")?,
                    result: row.decode("result")?,
                })
            })
            .transpose()?)
    }

    pub(crate) async fn runtime_run_source(&self, run: Uuid) -> Result<RuntimeRunSource> {
        let row=sql::record(&mut self.pool.clone(), "SELECT r.build_id,b.source_snapshot_id FROM planning_runs r JOIN builds b ON b.id=r.build_id WHERE r.id=$1 AND r.authority='kubernetes'", params![run], &[("build_id", Type::Uuid),("source_snapshot_id", Type::Uuid)]).await?;
        Ok(RuntimeRunSource {
            build_id: row.decode("build_id")?,
            snapshot_id: row.decode("source_snapshot_id")?,
        })
    }

    pub(crate) async fn source_snapshot_digest(&self, snapshot: Uuid) -> Result<String> {
        let digest: String = sql::scalar(
            &mut self.pool.clone(),
            "SELECT digest FROM source_snapshots WHERE id=$1",
            params![snapshot],
        )
        .await?;
        Ok(digest)
    }

    pub(crate) async fn commit_runtime_event<F, Fut>(
        &self,
        input: RuntimeEventCommit<'_>,
        prepare: F,
    ) -> Result<Value>
    where
        F: FnOnce(super::planning::SubmissionContext) -> Fut,
        Fut: std::future::Future<Output = Result<crate::execution_plan::ExecutionPlan>>,
    {
        let RuntimeEventCommit {
            event,
            run,
            build,
            hash,
            kind,
            payload,
            validation,
        } = input;
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;

        sql::execute(
            &mut tx,
            "SELECT id FROM builds WHERE id=$1 FOR UPDATE",
            params![build],
        )
        .await?;
        let row=sql::record(&mut tx, "SELECT r.*,b.project_id,b.source_snapshot_id,b.goal,b.target_requirements,b.planning_context,b.parameters,b.selected_targets,b.current_plan_id,b.revision_count,b.release_version,b.cancel_requested,b.status AS build_status FROM planning_runs r JOIN builds b ON b.id=r.build_id WHERE r.id=$1 FOR UPDATE OF r", params![run], &[("id", Type::Uuid),("build_id", Type::Uuid),("worker_id", Type::Uuid),("purpose", Type::String),("state", Type::String),("failed_targets", Type::List(Box::new(Type::String))),("model_config", Type::String),("lease_hash", Type::String),("lease_expires_at", Type::Timestamp),("deadline_at", Type::Timestamp),("calls", Type::I32),("usage", Type::String),("error", Type::String),("created_at", Type::Timestamp),("finished_at", Type::Timestamp),("authority", Type::String),("project_id", Type::Uuid),("source_snapshot_id", Type::Uuid),("goal", Type::String),("target_requirements", Type::String),("planning_context", Type::String),("parameters", Type::String),("selected_targets", Type::List(Box::new(Type::String))),("current_plan_id", Type::Uuid),("revision_count", Type::I32),("release_version", Type::String),("cancel_requested", Type::Bool),("build_status", Type::String)]).await?;
        if let Some(prior) = sql::optional_scalar::<Value>(
            &mut tx,
            "SELECT result FROM runtime_receipts WHERE event_id=$1",
            params![event],
        )
        .await?
        {
            return Ok(prior);
        }
        let existing: Option<Uuid> = sql::optional_scalar(
            &mut tx,
            "SELECT id FROM plan_revisions WHERE run_id=$1",
            params![run],
        )
        .await?;
        let result = if let Some(plan) = existing {
            json!({"accepted":true,"plan_revision_id":plan})
        } else if row.decode::<bool>("cancel_requested")?
            || !["planning", "replanning"].contains(&row.decode::<String>("build_status")?.as_str())
        {
            json!({"error":"build canceled or planning superseded"})
        } else if kind == "failed" {
            sql::execute(
                &mut tx,
                "UPDATE planning_runs SET state='failed',error=$2,finished_at=now() WHERE id=$1",
                params![
                    run,
                    payload["error"].as_str().unwrap_or("Pi planning failed")
                ],
            )
            .await?;
            sql::execute(
                &mut tx,
                "UPDATE builds SET status='failed',error=$2,finished_at=now() WHERE id=$1",
                params![
                    build,
                    payload["error"].as_str().unwrap_or("Pi planning failed")
                ],
            )
            .await?;
            json!({"ok":true})
        } else if kind != "proposal" {
            json!({"error":"unknown runtime event"})
        } else if payload["deadline_ms"]
            .as_i64()
            .is_none_or(|d| d <= chrono::Utc::now().timestamp_millis())
        {
            json!({"error":"planning deadline reached"})
        } else if let Some(error) = validation {
            json!({"error":error})
        } else if payload["kind"] == "request_input" {
            let question = payload["payload"]["question"].as_str().unwrap_or("");
            if question.trim().is_empty() || question.len() > 8000 {
                json!({"error":"a concise question is required"})
            } else {
                sql::execute(
                    &mut tx,
                    "UPDATE builds SET status='needs_input',clarification=$2 WHERE id=$1",
                    params![build, json!({"question":question})],
                )
                .await?;
                sql::execute(
                    &mut tx,
                    "UPDATE planning_runs SET state='needs_input',finished_at=now() WHERE id=$1",
                    params![run],
                )
                .await?;
                json!({"accepted":true,"needs_input":true})
            }
        } else if payload["kind"] == "submit_plan" {
            let mut savepoint = tx.transaction().await?;
            let result = async {
                let context = super::planning::submission_context(&mut savepoint, &row).await?;
                let plan = prepare(context).await?;
                super::planning::persist_plan(&mut savepoint, &row, plan).await
            }
            .await;
            match result {
                Ok(value) => {
                    savepoint.commit().await?;
                    value
                }
                Err(error) => {
                    savepoint.rollback().await?;
                    json!({"error":error.to_string()})
                }
            }
        } else {
            json!({"error":"invalid proposal kind"})
        };
        super::models::RuntimeReceipt::create()
            .event_id(event)
            .input_hash(hash)
            .result(toasty::Json(result.clone()))
            .run_id(run)
            .generation(payload["generation"].as_i64())
            .exec(&mut tx)
            .await?;
        super::models::PlanningEvent::create()
            .run_id(run)
            .kind("runtime_result")
            .payload(toasty::Json(result.clone()))
            .exec(&mut tx)
            .await?;
        tx.commit().await?;
        Ok(result)
    }

    pub(crate) async fn runtime_deliveries(&self) -> Result<Vec<RuntimeDelivery>> {
        let rows = sql::records(
            &mut self.pool.clone(),
            "SELECT * FROM runtime_intents WHERE delivered_at IS NULL ORDER BY id LIMIT 64",
            params![],
            &[
                ("id", Type::I64),
                ("run_id", Type::Uuid),
                ("version", Type::I64),
                ("kind", Type::String),
                ("payload_ciphertext", Type::String),
                ("delivered_at", Type::Timestamp),
                ("created_at", Type::Timestamp),
            ],
        )
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                Ok::<_, artifactory_database::Error>(RuntimeDelivery {
                    id: row.decode("id")?,
                    run_id: row.decode("run_id")?,
                    kind: row.decode("kind")?,
                    version: row.decode("version")?,
                    ciphertext: row.decode("payload_ciphertext")?,
                })
            })
            .collect::<artifactory_database::Result<_>>()?)
    }

    pub(crate) async fn save_runtime_payload(&self, event: i64, sealed: String) -> Result<String> {
        Ok(sql::scalar(&mut self.pool.clone(), "UPDATE runtime_intents SET payload_ciphertext=coalesce(payload_ciphertext,$2) WHERE id=$1 RETURNING payload_ciphertext", params![event, sealed]).await?)
    }

    pub(crate) async fn runtime_delivered(&self, event: i64) -> Result<()> {
        sql::execute(
            &mut self.pool.clone(),
            "UPDATE runtime_intents SET delivered_at=now() WHERE id=$1",
            params![event],
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn runtime_intent_context(&self, run: Uuid) -> Result<RuntimeIntentContext> {
        let row=sql::record(&mut self.pool.clone(), "SELECT r.*,b.project_id,b.source_snapshot_id,b.goal,b.requested_ref,b.commit_sha,b.parameters,b.target_requirements,b.planning_context,b.revision_count,ss.digest,ss.service_origin FROM planning_runs r JOIN builds b ON b.id=r.build_id JOIN source_snapshots ss ON ss.id=b.source_snapshot_id WHERE r.id=$1", params![run], &[("id", Type::Uuid),("build_id", Type::Uuid),("worker_id", Type::Uuid),("purpose", Type::String),("state", Type::String),("failed_targets", Type::List(Box::new(Type::String))),("model_config", Type::String),("lease_hash", Type::String),("lease_expires_at", Type::Timestamp),("deadline_at", Type::Timestamp),("calls", Type::I32),("usage", Type::String),("error", Type::String),("created_at", Type::Timestamp),("finished_at", Type::Timestamp),("authority", Type::String),("project_id", Type::Uuid),("source_snapshot_id", Type::Uuid),("goal", Type::String),("requested_ref", Type::String),("commit_sha", Type::String),("parameters", Type::String),("target_requirements", Type::String),("planning_context", Type::String),("revision_count", Type::I32),("digest", Type::String),("service_origin", Type::String)]).await?;
        let build: Uuid = row.decode("build_id")?;
        let current:Option<Value>=sql::optional_scalar(&mut self.pool.clone(), "SELECT contents FROM plan_revisions WHERE id=(SELECT current_plan_id FROM builds WHERE id=$1)", params![build]).await?;
        let failed:Vec<Value>=sql::scalars(&mut self.pool.clone(), "SELECT jsonb_build_object('target',j.target,'error',a.error,'attempt_id',a.id) FROM jobs j JOIN LATERAL (SELECT id,error FROM attempts WHERE job_id=j.id ORDER BY created_at DESC LIMIT 1) a ON true WHERE j.build_id=$1 AND j.status IN ('failed','lost')", params![build]).await?;
        let context = json!({"build_id":build,"goal":row.decode::<Option<String>>("goal")?,"ref":row.decode::<String>("requested_ref")?,"commit":row.decode::<Option<String>>("commit_sha")?,"parameters":row.decode::<Value>("parameters")?,"target_requirements":row.decode::<Value>("target_requirements")?,"project_context":row.decode::<Value>("planning_context")?,"snapshot_id":row.decode::<Uuid>("source_snapshot_id")?,"revision":row.decode::<i32>("revision_count")?,"current_plan":current,"failed_targets":failed,"nodes":self.planning_nodes(build).await?,"source_url":row.decode::<Option<String>>("service_origin")?});
        Ok(RuntimeIntentContext {
            build_id: build,
            project_id: row.decode("project_id")?,
            snapshot_id: row.decode("source_snapshot_id")?,
            digest: row.decode("digest")?,
            model: row.decode("model_config")?,
            context,
        })
    }

    pub(crate) async fn remote_capture_input(
        &self,
        build: Uuid,
    ) -> Result<Option<RemoteCaptureInput>> {
        let row=sql::optional_record(&mut self.pool.clone(), "SELECT b.*,b.source_repository_url AS repository_url,b.source_credential_id AS credential_id FROM builds b WHERE id=$1 AND planner_backend='kubernetes' AND status='resolving' AND NOT cancel_requested", params![build], &[("id", Type::Uuid),("project_id", Type::Uuid),("requested_ref", Type::String),("commit_sha", Type::String),("config_yaml", Type::String),("config_snapshot", Type::String),("parameters", Type::String),("selected_targets", Type::List(Box::new(Type::String))),("status", Type::String),("error", Type::String),("release_version", Type::String),("publish_status", Type::String),("publish_error", Type::String),("cancel_requested", Type::Bool),("requested_by", Type::Uuid),("created_at", Type::Timestamp),("finished_at", Type::Timestamp),("source_repository_url", Type::String),("source_credential_id", Type::Uuid),("goal", Type::String),("target_requirements", Type::String),("planning_context", Type::String),("model_config_snapshot", Type::String),("source_snapshot_id", Type::Uuid),("revision_count", Type::I32),("clarification", Type::String),("current_plan_id", Type::Uuid),("planner_backend", Type::String),("repository_url", Type::String),("credential_id", Type::Uuid)]).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let request = sql::record(
            &mut self.pool.clone(),
            "SELECT origin,payload_ciphertext FROM remote_source_requests WHERE build_id=$1",
            params![build],
            &[
                ("origin", Type::String),
                ("payload_ciphertext", Type::String),
            ],
        )
        .await?;
        let origin: String = request.decode("origin")?;
        Ok(Some(RemoteCaptureInput {
            project_id: row.decode("project_id")?,
            planning_context: row.decode("planning_context")?,
            credential_id: row.decode("credential_id")?,
            repository_url: row.decode("repository_url")?,
            requested_ref: row.decode("requested_ref")?,
            model_config_snapshot: row.decode("model_config_snapshot")?,
            origin,
            ciphertext: request.decode("payload_ciphertext")?,
        }))
    }

    pub(crate) async fn save_remote_source_payload(
        &self,
        build: Uuid,
        sealed: String,
    ) -> Result<String> {
        Ok(sql::scalar(&mut self.pool.clone(), "UPDATE remote_source_requests SET payload_ciphertext=coalesce(payload_ciphertext,$2) WHERE build_id=$1 RETURNING payload_ciphertext", params![build, sealed]).await?)
    }

    pub(crate) async fn save_remote_snapshot(&self, input: RemoteSnapshot<'_>) -> Result<()> {
        let RemoteSnapshot {
            build,
            project,
            snapshot,
            root_repository,
            result,
            origin,
            public_origin,
            model,
        } = input;
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        for repository in result["repositories"]
            .as_array()
            .context("repository metadata missing")?
        {
            sql::execute(&mut tx, "INSERT INTO source_repositories(id,project_id,repository_url,cache_key) VALUES($1,$2,$3,$4) ON CONFLICT(id) DO NOTHING", params![id(repository,"id")?, project, repository["repository_url"].as_str().context("repository URL missing")?, repository["cache_key"].as_str().context("cache key missing")?]).await?;
        }
        sql::execute(&mut tx, "INSERT INTO source_snapshots(id,project_id,root_repository_id,commit_sha,manifest,digest,service_origin,public_origin) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(id) DO NOTHING", params![snapshot.id, project, root_repository, &snapshot.commit, &result["snapshot"], result["digest"].as_str().context("snapshot digest missing")?, origin, public_origin]).await?;
        let changed=sql::execute(&mut tx, "UPDATE builds SET commit_sha=$2,source_snapshot_id=$3,status='planning' WHERE id=$1 AND status='resolving' AND NOT cancel_requested", params![build, &snapshot.commit, snapshot.id]).await?;
        if changed == 1 {
            super::models::PlanningRun::create()
                .id(Uuid::new_v4())
                .build_id(build)
                .purpose("initial")
                .model_config(toasty::Json(
                    (model.context("model configuration missing")?).clone(),
                ))
                .exec(&mut tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn remote_source_candidates(&self) -> Result<Vec<(Uuid, Uuid)>> {
        let candidates:Vec<(Uuid,Uuid)>=sql::tuples(&mut self.pool.clone(), "SELECT id,project_id FROM builds WHERE planner_backend='kubernetes' AND status='resolving' AND NOT cancel_requested AND goal IS NOT NULL ORDER BY created_at LIMIT 64", params![]).await?;
        Ok(candidates)
    }

    pub(crate) async fn route_source_project(&self, project: Uuid, origin: &str) -> Result<String> {
        Ok(sql::scalar(&mut self.pool.clone(), "INSERT INTO source_project_routes(project_id,origin) VALUES($1,$2) ON CONFLICT(project_id) DO UPDATE SET project_id=excluded.project_id RETURNING origin", params![project, origin]).await?)
    }

    pub(crate) async fn claim_remote_source(&self, build: Uuid, origin: String) -> Result<bool> {
        super::models::RemoteSourceRequest::upsert_by_build_id(build)
            .origin(origin)
            .or_ignore()
            .exec(&mut self.pool.clone())
            .await?;
        let claimed=sql::execute(&mut self.pool.clone(), "UPDATE remote_source_requests SET lease_until=now()+interval '360 seconds' WHERE build_id=$1 AND (lease_until IS NULL OR lease_until<now())", params![build]).await?;
        if claimed == 0 {
            return Ok(false);
        }
        Ok(true)
    }

    pub(crate) async fn finish_remote_source(&self, build: Uuid, failed: bool) -> Result<()> {
        sql::execute(&mut self.pool.clone(), "UPDATE remote_source_requests SET lease_until=now()+interval '3 seconds',error=$2 WHERE build_id=$1", params![build, if failed{Some("source service unavailable or capture incomplete")}else{None}]).await?;
        // Recoverable source/network faults are retried against the saved operation, bounded to 20 minutes.
        if failed {
            sql::execute(&mut self.pool.clone(), "UPDATE builds SET status='failed',error='source capture exceeded provisioning deadline',finished_at=now() WHERE id=$1 AND status='resolving' AND created_at<now()-interval '20 minutes'", params![build]).await?;
        }
        Ok(())
    }
}
