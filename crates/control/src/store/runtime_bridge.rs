//! Durable runtime receipts, outbox delivery and remote source state.
use super::ControlStore;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use sqlx::{Acquire, Row};
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
        let origin: Option<String> =
            sqlx::query_scalar("SELECT origin FROM source_project_routes WHERE project_id=$1")
                .bind(project)
                .fetch_optional(&self.pool)
                .await?;
        Ok(origin)
    }

    pub(crate) async fn source_origin(&self, snapshot: Uuid) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT service_origin FROM source_snapshots WHERE id=$1")
                .bind(snapshot)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub(crate) async fn source_download_origin(&self, snapshot: Uuid) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT coalesce(public_origin,service_origin) FROM source_snapshots WHERE id=$1",
        )
        .bind(snapshot)
        .fetch_one(&self.pool)
        .await?)
    }

    pub(crate) async fn authorize_source_download(
        &self,
        token_hash: String,
        snapshot: Uuid,
    ) -> Result<bool> {
        let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM source_tokens t JOIN attempts a ON a.id=t.attempt_id JOIN nodes n ON n.id=a.node_id JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id JOIN source_snapshots ss ON ss.id=t.snapshot_id WHERE t.token_hash=$1 AND t.snapshot_id=$2 AND b.source_snapshot_id=ss.id AND b.project_id=ss.project_id AND t.expires_at>clock_timestamp() AND a.status IN ('leased','running') AND a.lease_expires_at>clock_timestamp() AND NOT b.cancel_requested AND NOT n.revoked AND n.certificate_expires_at>clock_timestamp())").bind(token_hash).bind(snapshot).fetch_one(&self.pool).await?;
        Ok(valid)
    }

    pub(crate) async fn runtime_tool_build(&self, run: Uuid) -> Result<Uuid> {
        let build:Uuid=sqlx::query_scalar("SELECT b.id FROM builds b JOIN planning_runs r ON r.build_id=b.id WHERE r.id=$1 AND r.authority='kubernetes' AND NOT b.cancel_requested AND b.status IN ('planning','replanning')").bind(run).fetch_one(&self.pool).await?;
        Ok(build)
    }

    pub(crate) async fn runtime_receipt(&self, event: Uuid) -> Result<Option<RuntimeReceipt>> {
        let row = sqlx::query("SELECT input_hash,result FROM runtime_receipts WHERE event_id=$1")
            .bind(event)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| RuntimeReceipt {
            input_hash: row.get("input_hash"),
            result: row.get("result"),
        }))
    }

    pub(crate) async fn runtime_run_source(&self, run: Uuid) -> Result<RuntimeRunSource> {
        let row=sqlx::query("SELECT r.build_id,b.source_snapshot_id FROM planning_runs r JOIN builds b ON b.id=r.build_id WHERE r.id=$1 AND r.authority='kubernetes'").bind(run).fetch_one(&self.pool).await?;
        Ok(RuntimeRunSource {
            build_id: row.get("build_id"),
            snapshot_id: row.get("source_snapshot_id"),
        })
    }

    pub(crate) async fn source_snapshot_digest(&self, snapshot: Uuid) -> Result<String> {
        let digest: String = sqlx::query_scalar("SELECT digest FROM source_snapshots WHERE id=$1")
            .bind(snapshot)
            .fetch_one(&self.pool)
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
        let mut tx = self.pool.begin().await?;

        sqlx::query("SELECT id FROM builds WHERE id=$1 FOR UPDATE")
            .bind(build)
            .execute(&mut *tx)
            .await?;
        let row=sqlx::query("SELECT r.*,b.project_id,b.source_snapshot_id,b.goal,b.target_requirements,b.planning_context,b.parameters,b.selected_targets,b.current_plan_id,b.revision_count,b.release_version,b.cancel_requested,b.status AS build_status FROM planning_runs r JOIN builds b ON b.id=r.build_id WHERE r.id=$1 FOR UPDATE OF r").bind(run).fetch_one(&mut *tx).await?;
        if let Some(prior) =
            sqlx::query_scalar::<_, Value>("SELECT result FROM runtime_receipts WHERE event_id=$1")
                .bind(event)
                .fetch_optional(&mut *tx)
                .await?
        {
            return Ok(prior);
        }
        let existing: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM plan_revisions WHERE run_id=$1")
                .bind(run)
                .fetch_optional(&mut *tx)
                .await?;
        let result = if let Some(plan) = existing {
            json!({"accepted":true,"plan_revision_id":plan})
        } else if row.get::<bool, _>("cancel_requested")
            || !["planning", "replanning"].contains(&row.get::<String, _>("build_status").as_str())
        {
            json!({"error":"build canceled or planning superseded"})
        } else if kind == "failed" {
            sqlx::query(
                "UPDATE planning_runs SET state='failed',error=$2,finished_at=now() WHERE id=$1",
            )
            .bind(run)
            .bind(payload["error"].as_str().unwrap_or("Pi planning failed"))
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE builds SET status='failed',error=$2,finished_at=now() WHERE id=$1")
                .bind(build)
                .bind(payload["error"].as_str().unwrap_or("Pi planning failed"))
                .execute(&mut *tx)
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
                sqlx::query("UPDATE builds SET status='needs_input',clarification=$2 WHERE id=$1")
                    .bind(build)
                    .bind(json!({"question":question}))
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(
                    "UPDATE planning_runs SET state='needs_input',finished_at=now() WHERE id=$1",
                )
                .bind(run)
                .execute(&mut *tx)
                .await?;
                json!({"accepted":true,"needs_input":true})
            }
        } else if payload["kind"] == "submit_plan" {
            let mut savepoint = tx.begin().await?;
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
        sqlx::query("INSERT INTO runtime_receipts(event_id,input_hash,result,run_id,generation) VALUES($1,$2,$3,$4,$5)").bind(event).bind(hash).bind(&result).bind(run).bind(payload["generation"].as_i64()).execute(&mut *tx).await?;
        sqlx::query(
            "INSERT INTO planning_events(run_id,kind,payload) VALUES($1,'runtime_result',$2)",
        )
        .bind(run)
        .bind(&result)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    pub(crate) async fn runtime_deliveries(&self) -> Result<Vec<RuntimeDelivery>> {
        let rows = sqlx::query(
            "SELECT * FROM runtime_intents WHERE delivered_at IS NULL ORDER BY id LIMIT 64",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| RuntimeDelivery {
                id: row.get("id"),
                run_id: row.get("run_id"),
                kind: row.get("kind"),
                version: row.get("version"),
                ciphertext: row.get("payload_ciphertext"),
            })
            .collect())
    }

    pub(crate) async fn save_runtime_payload(&self, event: i64, sealed: String) -> Result<String> {
        Ok(sqlx::query_scalar("UPDATE runtime_intents SET payload_ciphertext=coalesce(payload_ciphertext,$2) WHERE id=$1 RETURNING payload_ciphertext").bind(event).bind(sealed).fetch_one(&self.pool).await?)
    }

    pub(crate) async fn runtime_delivered(&self, event: i64) -> Result<()> {
        sqlx::query("UPDATE runtime_intents SET delivered_at=now() WHERE id=$1")
            .bind(event)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub(crate) async fn runtime_intent_context(&self, run: Uuid) -> Result<RuntimeIntentContext> {
        let row=sqlx::query("SELECT r.*,b.project_id,b.source_snapshot_id,b.goal,b.requested_ref,b.commit_sha,b.parameters,b.target_requirements,b.planning_context,b.revision_count,ss.digest,ss.service_origin FROM planning_runs r JOIN builds b ON b.id=r.build_id JOIN source_snapshots ss ON ss.id=b.source_snapshot_id WHERE r.id=$1").bind(run).fetch_one(&self.pool).await?;
        let build: Uuid = row.get("build_id");
        let current:Option<Value>=sqlx::query_scalar("SELECT contents FROM plan_revisions WHERE id=(SELECT current_plan_id FROM builds WHERE id=$1)").bind(build).fetch_optional(&self.pool).await?;
        let failed:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('target',j.target,'error',a.error,'attempt_id',a.id) FROM jobs j JOIN LATERAL (SELECT id,error FROM attempts WHERE job_id=j.id ORDER BY created_at DESC LIMIT 1) a ON true WHERE j.build_id=$1 AND j.status IN ('failed','lost')").bind(build).fetch_all(&self.pool).await?;
        let context = json!({"build_id":build,"goal":row.get::<Option<String>,_>("goal"),"ref":row.get::<String,_>("requested_ref"),"commit":row.get::<Option<String>,_>("commit_sha"),"parameters":row.get::<Value,_>("parameters"),"target_requirements":row.get::<Value,_>("target_requirements"),"project_context":row.get::<Value,_>("planning_context"),"snapshot_id":row.get::<Uuid,_>("source_snapshot_id"),"revision":row.get::<i32,_>("revision_count"),"current_plan":current,"failed_targets":failed,"nodes":self.planning_nodes(build).await?,"source_url":row.get::<Option<String>,_>("service_origin")});
        Ok(RuntimeIntentContext {
            build_id: build,
            project_id: row.get("project_id"),
            snapshot_id: row.get("source_snapshot_id"),
            digest: row.get("digest"),
            model: row.get("model_config"),
            context,
        })
    }

    pub(crate) async fn remote_capture_input(
        &self,
        build: Uuid,
    ) -> Result<Option<RemoteCaptureInput>> {
        let row=sqlx::query("SELECT b.*,b.source_repository_url AS repository_url,b.source_credential_id AS credential_id FROM builds b WHERE id=$1 AND planner_backend='kubernetes' AND status='resolving' AND NOT cancel_requested").bind(build).fetch_optional(&self.pool).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let request = sqlx::query(
            "SELECT origin,payload_ciphertext FROM remote_source_requests WHERE build_id=$1",
        )
        .bind(build)
        .fetch_one(&self.pool)
        .await?;
        let origin: String = request.get("origin");
        Ok(Some(RemoteCaptureInput {
            project_id: row.get("project_id"),
            planning_context: row.get("planning_context"),
            credential_id: row.get("credential_id"),
            repository_url: row.get("repository_url"),
            requested_ref: row.get("requested_ref"),
            model_config_snapshot: row.get("model_config_snapshot"),
            origin,
            ciphertext: request.get("payload_ciphertext"),
        }))
    }

    pub(crate) async fn save_remote_source_payload(
        &self,
        build: Uuid,
        sealed: String,
    ) -> Result<String> {
        Ok(sqlx::query_scalar("UPDATE remote_source_requests SET payload_ciphertext=coalesce(payload_ciphertext,$2) WHERE build_id=$1 RETURNING payload_ciphertext").bind(build).bind(sealed).fetch_one(&self.pool).await?)
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
        let mut tx = self.pool.begin().await?;
        for repository in result["repositories"]
            .as_array()
            .context("repository metadata missing")?
        {
            sqlx::query("INSERT INTO source_repositories(id,project_id,repository_url,cache_key) VALUES($1,$2,$3,$4) ON CONFLICT(id) DO NOTHING").bind(id(repository,"id")?).bind(project).bind(repository["repository_url"].as_str().context("repository URL missing")?).bind(repository["cache_key"].as_str().context("cache key missing")?).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO source_snapshots(id,project_id,root_repository_id,commit_sha,manifest,digest,service_origin,public_origin) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(id) DO NOTHING").bind(snapshot.id).bind(project).bind(root_repository).bind(&snapshot.commit).bind(&result["snapshot"]).bind(result["digest"].as_str().context("snapshot digest missing")?).bind(origin).bind(public_origin).execute(&mut *tx).await?;
        let changed=sqlx::query("UPDATE builds SET commit_sha=$2,source_snapshot_id=$3,status='planning' WHERE id=$1 AND status='resolving' AND NOT cancel_requested").bind(build).bind(&snapshot.commit).bind(snapshot.id).execute(&mut *tx).await?;
        if changed.rows_affected() == 1 {
            sqlx::query("INSERT INTO planning_runs(id,build_id,purpose,model_config) VALUES($1,$2,'initial',$3)").bind(Uuid::new_v4()).bind(build).bind(model.context("model configuration missing")?).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn remote_source_candidates(&self) -> Result<Vec<(Uuid, Uuid)>> {
        let candidates:Vec<(Uuid,Uuid)>=sqlx::query_as("SELECT id,project_id FROM builds WHERE planner_backend='kubernetes' AND status='resolving' AND NOT cancel_requested AND goal IS NOT NULL ORDER BY created_at LIMIT 64").fetch_all(&self.pool).await?;
        Ok(candidates)
    }

    pub(crate) async fn route_source_project(&self, project: Uuid, origin: &str) -> Result<String> {
        Ok(sqlx::query_scalar("INSERT INTO source_project_routes(project_id,origin) VALUES($1,$2) ON CONFLICT(project_id) DO UPDATE SET project_id=excluded.project_id RETURNING origin").bind(project).bind(origin).fetch_one(&self.pool).await?)
    }

    pub(crate) async fn claim_remote_source(&self, build: Uuid, origin: String) -> Result<bool> {
        sqlx::query("INSERT INTO remote_source_requests(build_id,origin) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(build).bind(origin).execute(&self.pool).await?;
        let claimed=sqlx::query("UPDATE remote_source_requests SET lease_until=now()+interval '360 seconds' WHERE build_id=$1 AND (lease_until IS NULL OR lease_until<now())").bind(build).execute(&self.pool).await?;
        if claimed.rows_affected() == 0 {
            return Ok(false);
        }
        Ok(true)
    }

    pub(crate) async fn finish_remote_source(&self, build: Uuid, failed: bool) -> Result<()> {
        sqlx::query("UPDATE remote_source_requests SET lease_until=now()+interval '3 seconds',error=$2 WHERE build_id=$1").bind(build).bind(if failed{Some("source service unavailable or capture incomplete")}else{None}).execute(&self.pool).await?;
        // Recoverable source/network faults are retried against the saved operation, bounded to 20 minutes.
        if failed {
            sqlx::query("UPDATE builds SET status='failed',error='source capture exceeded provisioning deadline',finished_at=now() WHERE id=$1 AND status='resolving' AND created_at<now()-interval '20 minutes'").bind(build).execute(&self.pool).await?;
        }
        Ok(())
    }
}
