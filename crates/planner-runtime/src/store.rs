//! Runtime persistence. Lifecycle rules execute with the locked persistent state.
use crate::{lifecycle::Lifecycle, server::id};
use anyhow::{Context, Result, ensure};
use artifactory_runtime_protocol::{Execution, Lease, RunIntent, crypto};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::{Row, postgres::PgRow};
use std::sync::Arc;
use uuid::Uuid;
#[derive(Clone)]
pub struct RuntimeStore {
    pool: sqlx::PgPool,
    key: Arc<[u8; 32]>,
}
pub struct RuntimeMetrics {
    pub statuses: Vec<(String, i64)>,
    pub calls: i64,
    pub pending: i64,
    pub oldest: f64,
    pub outbox: i64,
}
pub enum ToolDisposition {
    Immediate(Value),
    Forward(Value),
}
pub struct OutboxDelivery {
    pub id: Uuid,
    pub run_id: Uuid,
    pub kind: String,
    pub payload: Value,
}
pub struct ModelRequest<'a> {
    pub run: Uuid,
    pub generation: i64,
    pub lease: &'a Lease,
    pub request_id: Uuid,
    pub path: &'a str,
    pub value: &'a Value,
    pub hash: String,
}
pub enum ModelReservation {
    Duplicate,
    Reserved {
        config: artifactory_runtime_protocol::ModelConfiguration,
        deadline_ms: i64,
        wait_ms: i64,
        endpoint: String,
    },
}

async fn locked(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, run: Uuid) -> Result<PgRow> {
    Ok(
        sqlx::query("SELECT * FROM runtime_runs WHERE id=$1 FOR UPDATE")
            .bind(run)
            .fetch_one(&mut **tx)
            .await?,
    )
}
fn config(s: &RuntimeStore, row: &PgRow) -> Result<RunIntent> {
    Ok(serde_json::from_slice(&crypto::open(
        &s.key,
        &row.get::<String, _>("intent_ciphertext"),
    )?)?)
}
fn authorized(row: &PgRow, lease: &Lease) -> Result<Lifecycle> {
    ensure!(
        row.get::<Option<String>, _>("grant_hash").as_deref() == Some(&crypto::hash(&lease.token)),
        "invalid run grant"
    );
    let life: Lifecycle = serde_json::from_value(row.get("lifecycle"))?;
    life.valid(lease.generation, &lease.uid, Utc::now().timestamp_millis())?;
    ensure!(
        !["succeeded", "needs_input", "failed", "canceled"]
            .contains(&row.get::<String, _>("status").as_str()),
        "run is terminal"
    );
    Ok(life)
}
async fn save(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run: Uuid,
    life: &Lifecycle,
) -> Result<()> {
    sqlx::query("UPDATE runtime_runs SET lifecycle=$2,updated_at=now() WHERE id=$1")
        .bind(run)
        .bind(serde_json::to_value(life)?)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn enqueue(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run: Uuid,
    kind: &str,
    payload: Value,
) -> Result<Uuid> {
    let event = Uuid::new_v4();
    sqlx::query("INSERT INTO runtime_outbox(id,run_id,kind,payload) VALUES($1,$2,$3,$4)")
        .bind(event)
        .bind(run)
        .bind(kind)
        .bind(payload)
        .execute(&mut **tx)
        .await?;
    Ok(event)
}
impl RuntimeStore {
    pub fn new(pool: sqlx::PgPool, key: Arc<[u8; 32]>) -> Self {
        Self { pool, key }
    }
    pub async fn metrics(&self) -> Result<RuntimeMetrics> {
        let rows = sqlx::query("SELECT status,count(*) AS runs FROM runtime_runs GROUP BY status")
            .fetch_all(&self.pool)
            .await?;
        let (calls,pending,oldest):(i64,i64,f64)=sqlx::query_as("SELECT coalesce(sum((lifecycle->>'calls')::bigint),0)::bigint,count(*) FILTER(WHERE (lifecycle->>'pending')::bool),coalesce(max(extract(epoch FROM now()-created_at)) FILTER(WHERE status='queued'),0)::float8 FROM runtime_runs").fetch_one(&self.pool).await?;
        let outbox: i64 =
            sqlx::query_scalar("SELECT count(*) FROM runtime_outbox WHERE NOT delivered")
                .fetch_one(&self.pool)
                .await?;
        Ok(RuntimeMetrics {
            statuses: rows
                .into_iter()
                .map(|row| (row.get("status"), row.get("runs")))
                .collect(),
            calls,
            pending,
            oldest,
            outbox,
        })
    }

    pub async fn put_intent(
        &self,
        run: Uuid,
        version: i64,
        hash: String,
        cancel: bool,
        intent: Option<RunIntent>,
        ciphertext: Option<String>,
    ) -> Result<Value> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(run.to_string())
            .execute(&mut *tx)
            .await?;
        if let Some(row) = sqlx::query(
            "SELECT version,intent_hash,status FROM runtime_runs WHERE id=$1 FOR UPDATE",
        )
        .bind(run)
        .fetch_optional(&mut *tx)
        .await?
        {
            if row.get::<i64, _>("version") > version {
                return Ok(json!({"ok":true,"stale":true}));
            }
            if row.get::<i64, _>("version") == version {
                ensure!(
                    row.get::<Option<String>, _>("intent_hash").as_deref() == Some(&hash),
                    "intent ID/version reused with different input"
                );
                return Ok(json!({"ok":true}));
            }
            ensure!(cancel, "running intent is immutable");
            sqlx::query("UPDATE runtime_runs SET version=$2,intent_hash=$3,status='canceled',lifecycle=jsonb_set(lifecycle,'{canceled}','true'),updated_at=now() WHERE id=$1").bind(run).bind(version).bind(hash).execute(&mut *tx).await?;
        } else {
            let mut life = Lifecycle::new(intent.as_ref().map_or(20, |i| i.model.max_calls));
            life.canceled = cancel;
            sqlx::query("INSERT INTO runtime_runs(id,version,intent_hash,intent_ciphertext,project_id,pool,snapshot_id,snapshot_digest,lifecycle,status) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
   .bind(run).bind(version).bind(hash).bind(ciphertext).bind(intent.as_ref().map(|i|i.project_id)).bind(intent.as_ref().map(|i|&i.pool)).bind(intent.as_ref().map(|i|i.snapshot_id)).bind(intent.as_ref().map(|i|&i.snapshot_digest)).bind(serde_json::to_value(life)?).bind(if cancel{"canceled"}else{"queued"}).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(json!({"ok":true}))
    }

    pub async fn register_execution(&self, execution: Execution) -> Result<Value> {
        let mut tx = self.pool.begin().await?;
        let row = locked(&mut tx, execution.run_id).await?;
        let life: Lifecycle = serde_json::from_value(row.get("lifecycle"))?;
        ensure!(
            !life.canceled && life.generation == execution.generation,
            "execution fenced"
        );
        let existing: Option<Value> = row.get("execution");
        if let Some(existing) = existing {
            ensure!(
                existing == serde_json::to_value(&execution)?,
                "execution identity already registered"
            );
        } else {
            sqlx::query("UPDATE runtime_runs SET execution=$2 WHERE id=$1")
                .bind(execution.run_id)
                .bind(serde_json::to_value(execution)?)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(json!({"ok":true}))
    }

    pub async fn heartbeat_run(&self, lease: Lease) -> Result<Value> {
        let mut tx = self.pool.begin().await?;
        let row = locked(&mut tx, lease.run_id).await?;
        let mut life = authorized(&row, &lease)?;
        life.lease_until_ms =
            Some((Utc::now().timestamp_millis() + 90_000).min(life.deadline_ms.unwrap()));
        save(&mut tx, lease.run_id, &life).await?;
        tx.commit().await?;
        Ok(json!({"ok":true}))
    }

    pub async fn validate_grant(&self, lease: Lease) -> Result<Value> {
        let row = sqlx::query("SELECT * FROM runtime_runs WHERE id=$1")
            .bind(lease.run_id)
            .fetch_one(&self.pool)
            .await?;
        let life = authorized(&row, &lease)?;
        ensure!(!life.pending, "proposal awaiting acceptance");
        Ok(
            json!({"snapshot_id":row.get::<Uuid,_>("snapshot_id"),"project_id":row.get::<Uuid,_>("project_id")}),
        )
    }

    pub async fn read_run(&self, run: Uuid) -> Result<Value> {
        let row = sqlx::query("SELECT * FROM runtime_runs WHERE id=$1")
            .bind(run)
            .fetch_one(&self.pool)
            .await?;
        Ok(
            json!({"run_id":run,"status":row.get::<String,_>("status"),"lifecycle":row.get::<Value,_>("lifecycle"),"execution":row.get::<Option<Value>,_>("execution")}),
        )
    }

    pub async fn report_event(&self, v: Value) -> Result<Value> {
        let lease: Lease = serde_json::from_value(v["lease"].clone())?;
        let event = id(&v, "event_id")?;
        let mut tx = self.pool.begin().await?;
        let row = locked(&mut tx, lease.run_id).await?;
        authorized(&row, &lease)?;
        let hash = crypto::hash(serde_json::to_vec(&v)?);
        let prior: Option<String> = sqlx::query_scalar(
            "SELECT input_hash FROM runtime_reports WHERE run_id=$1 AND event_id=$2",
        )
        .bind(lease.run_id)
        .bind(event)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(prior) = prior {
            ensure!(prior == hash, "event ID reused");
            return Ok(json!({"ok":true}));
        }
        let kind = v["kind"].as_str().context("event kind required")?;
        ensure!(
            matches!(kind, "checkpoint" | "usage" | "failed"),
            "unsupported event kind"
        );
        let sequence = v["sequence"].as_i64().context("event sequence required")?;
        if sequence > row.get::<i64, _>("report_sequence") {
            if kind == "checkpoint" {
                sqlx::query("UPDATE runtime_runs SET checkpoint=$2,report_sequence=$3 WHERE id=$1")
                    .bind(lease.run_id)
                    .bind(&v["payload"])
                    .bind(sequence)
                    .execute(&mut *tx)
                    .await?;
            }
            if kind == "usage" {
                sqlx::query("UPDATE runtime_runs SET usage=$2,report_sequence=$3 WHERE id=$1")
                    .bind(lease.run_id)
                    .bind(&v["payload"])
                    .bind(sequence)
                    .execute(&mut *tx)
                    .await?;
            }
            if kind == "failed" {
                sqlx::query("UPDATE runtime_runs SET status='failed' WHERE id=$1")
                    .bind(lease.run_id)
                    .execute(&mut *tx)
                    .await?;
                enqueue(&mut tx,lease.run_id,"failed",json!({"error":"Pi planning failed","calls":serde_json::from_value::<Lifecycle>(row.get("lifecycle"))?.calls})).await?;
            }
        }
        sqlx::query("INSERT INTO runtime_reports(run_id,event_id,input_hash) VALUES($1,$2,$3)")
            .bind(lease.run_id)
            .bind(event)
            .bind(hash)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(json!({"ok":true}))
    }

    pub async fn acquire_execution(&self, execution: Execution, public_url: &str) -> Result<Value> {
        let mut tx = self.pool.begin().await?;
        let row = locked(&mut tx, execution.run_id).await?;
        ensure!(
            matches!(
                row.get::<String, _>("status").as_str(),
                "provisioning" | "running"
            ),
            "run is not admitted for execution"
        );
        ensure!(
            row.get::<Option<Value>, _>("execution") == Some(serde_json::to_value(&execution)?),
            "unregistered Pod UID"
        );
        let mut life: Lifecycle = serde_json::from_value(row.get("lifecycle"))?;
        let intent = config(self, &row)?;
        life.acquire(
            execution.generation,
            &execution.uid,
            Utc::now().timestamp_millis(),
            i64::from(intent.model.timeout_seconds) * 1000,
        )?;
        let token = if let Some(ciphertext) = row.get::<Option<String>, _>("grant_ciphertext") {
            String::from_utf8(crypto::open(&self.key, &ciphertext)?)?
        } else {
            crypto::token()
        };
        sqlx::query("UPDATE runtime_runs SET status='running',lifecycle=$2,grant_hash=$3,grant_ciphertext=$4 WHERE id=$1").bind(execution.run_id).bind(serde_json::to_value(&life)?).bind(crypto::hash(&token)).bind(crypto::seal(&self.key,token.as_bytes())?).execute(&mut *tx).await?;
        let source_url = intent.context.get("source_url").cloned();
        let mut context = intent.context;
        context["checkpoint"] = row
            .get::<Option<Value>, _>("checkpoint")
            .unwrap_or(Value::Null);
        context["usage"] = row.get::<Value, _>("usage");
        let rejected:Option<Value>=sqlx::query_scalar("SELECT jsonb_build_object('kind',kind,'payload',payload,'result',result) FROM runtime_proposals WHERE run_id=$1 AND result IS NOT NULL ORDER BY created_at DESC LIMIT 1").bind(execution.run_id).fetch_optional(&mut *tx).await?;
        if let Some(value) = rejected {
            context["recovered_proposal"] = value;
        }
        let mut model = intent.model;
        model.api_key = token.clone();
        model.base_url = format!(
            "{}/model/{}/{}/{}/v1",
            public_url, execution.run_id, execution.generation, execution.uid
        );
        let lease = Lease {
            run_id: execution.run_id,
            generation: execution.generation,
            uid: execution.uid,
            token,
        };
        let result = json!({"lease":lease,"task":{"context":context,"model":model,"max_calls":life.max_calls.saturating_sub(life.calls),"timeout_seconds":((life.deadline_ms.unwrap()-Utc::now().timestamp_millis()).max(1)+999)/1000,"proxied":true},"source_url":source_url});
        tx.commit().await?;
        Ok(result)
    }

    pub async fn tool_call(
        &self,
        lease: Lease,
        name: &str,
        proposal: Uuid,
        v: &Value,
    ) -> Result<ToolDisposition> {
        let mut tx = self.pool.begin().await?;
        let row = locked(&mut tx, lease.run_id).await?;
        let prior = sqlx::query(
            "SELECT input_hash,result FROM runtime_proposals WHERE id=$1 AND run_id=$2",
        )
        .bind(proposal)
        .bind(lease.run_id)
        .fetch_optional(&mut *tx)
        .await?;
        let hash = crypto::hash(serde_json::to_vec(
            &json!({"name":name,"arguments":v["arguments"]}),
        )?);
        if let Some(prior) = prior {
            ensure!(
                prior.get::<String, _>("input_hash") == hash,
                "proposal ID reused"
            );
            return Ok(ToolDisposition::Immediate(
                prior
                    .get::<Option<Value>, _>("result")
                    .unwrap_or(json!({"pending":true,"proposal_id":proposal})),
            ));
        }
        let mut life = authorized(&row, &lease)?;
        ensure!(!life.pending, "proposal awaiting acceptance");
        if matches!(name, "submit_plan" | "request_input") {
            life.pending = true;
            sqlx::query("INSERT INTO runtime_proposals(id,run_id,generation,input_hash,kind,payload,deadline_ms) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(proposal).bind(lease.run_id).bind(lease.generation).bind(hash).bind(name).bind(&v["arguments"]).bind(life.deadline_ms.unwrap()).execute(&mut *tx).await?;
            save(&mut tx, lease.run_id, &life).await?;
            enqueue(&mut tx,lease.run_id,"proposal",json!({"proposal_id":proposal,"generation":lease.generation,"kind":name,"payload":v["arguments"],"deadline_ms":life.deadline_ms,"snapshot_digest":row.get::<String,_>("snapshot_digest")})).await?;
            tx.commit().await?;
            Ok(ToolDisposition::Immediate(
                json!({"pending":true,"proposal_id":proposal}),
            ))
        } else {
            ensure!(
                matches!(name, "node_capabilities" | "read_logs"),
                "source tools must use Source Service"
            );
            tx.commit().await?;
            Ok(ToolDisposition::Forward(
                json!({"run_id":lease.run_id,"name":name,"arguments":v["arguments"]}),
            ))
        }
    }

    pub async fn proposal_result(&self, proposal: Uuid, lease: Lease) -> Result<Value> {
        let row=sqlx::query("SELECT p.result FROM runtime_proposals p JOIN runtime_runs r ON r.id=p.run_id WHERE p.id=$1 AND p.run_id=$2 AND r.grant_hash=$3 AND p.generation=$4").bind(proposal).bind(lease.run_id).bind(crypto::hash(lease.token)).bind(lease.generation).fetch_one(&self.pool).await?;
        Ok(row
            .get::<Option<Value>, _>("result")
            .unwrap_or(json!({"pending":true})))
    }

    pub async fn queue(&self, pool: &str, limit: i64, per_project: i64) -> Result<Value> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,1))")
            .bind(pool)
            .execute(&mut *tx)
            .await?;
        let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM runtime_runs WHERE pool=$1 AND status IN ('provisioning','running')",
    )
    .bind(pool)
    .fetch_one(&mut *tx)
    .await?;
        let rows=sqlx::query("SELECT * FROM runtime_runs r WHERE pool=$1 AND status='queued' AND (SELECT count(*) FROM runtime_runs a WHERE a.project_id=r.project_id AND a.pool=r.pool AND a.status IN ('provisioning','running'))<$3 ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT $2").bind(pool).bind((limit-active).max(0)).bind(per_project).fetch_all(&mut *tx).await?;
        let mut admitted = Vec::new();
        for row in rows {
            let project: Uuid = row.get("project_id");
            let count:i64=sqlx::query_scalar("SELECT count(*) FROM runtime_runs WHERE project_id=$1 AND pool=$2 AND status IN ('provisioning','running')").bind(project).bind(pool).fetch_one(&mut *tx).await?;
            if count >= per_project {
                continue;
            }
            let run: Uuid = row.get("id");
            let life: Lifecycle = serde_json::from_value(row.get("lifecycle"))?;
            sqlx::query(
                "UPDATE runtime_runs SET status='provisioning',dispatched_at=now() WHERE id=$1",
            )
            .bind(run)
            .execute(&mut *tx)
            .await?;
            admitted.push(json!({"run_id":run,"generation":life.generation,"pool":pool,"snapshot_id":row.get::<Uuid,_>("snapshot_id"),"snapshot_digest":row.get::<String,_>("snapshot_digest")}));
        }
        // Lost create acknowledgements are recovered by returning all admitted descriptors.
        let active = sqlx::query(
            "SELECT * FROM runtime_runs WHERE pool=$1 AND status IN ('provisioning','running')",
        )
        .bind(pool)
        .fetch_all(&mut *tx)
        .await?;
        let mut rows = Vec::new();
        for row in active {
            let intent = config(self, &row)?;
            let lifecycle: Lifecycle = serde_json::from_value(row.get("lifecycle"))?;
            rows.push(json!({"run_id":intent.run_id,"generation":lifecycle.generation,"pool":intent.pool,"snapshot_id":intent.snapshot_id,"snapshot_digest":intent.snapshot_digest,"source_url":intent.context["source_url"]}));
        }
        tx.commit().await?;
        let queued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM runtime_runs WHERE pool=$1 AND status='queued'",
        )
        .bind(pool)
        .fetch_one(&self.pool)
        .await?;
        Ok(json!({"active":rows.len(),"queued":queued,"runs":rows}))
    }

    pub async fn maintain(&self) -> Result<()> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM runtime_runs WHERE status IN ('provisioning','running')",
        )
        .fetch_all(&self.pool)
        .await?;
        for id in ids {
            let mut tx = self.pool.begin().await?;
            let row = locked(&mut tx, id).await?;
            let mut life: Lifecycle = serde_json::from_value(row.get("lifecycle"))?;
            let now = Utc::now().timestamp_millis();
            if !matches!(
                row.get::<String, _>("status").as_str(),
                "provisioning" | "running"
            ) {
                continue;
            }
            if life.pending {
                continue;
            }
            let expired = life.deadline_ms.is_some_and(|d| d <= now) || life.generation >= 4;
            let stalled = row.get::<String, _>("status") == "provisioning"
                && row
                    .get::<Option<chrono::DateTime<Utc>>, _>("dispatched_at")
                    .is_some_and(|t| t < Utc::now() - chrono::Duration::seconds(180));
            if expired || (stalled && life.generation >= 3) {
                sqlx::query("UPDATE runtime_runs SET status='failed' WHERE id=$1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                enqueue(&mut tx,id,"failed",json!({"error":"planning deadline or provisioning retry limit reached","calls":life.calls})).await?;
            } else if stalled || life.lease_until_ms.is_some_and(|until| until <= now) {
                life.retry(now)?;
                save(&mut tx, id, &life).await?;
                sqlx::query("UPDATE runtime_runs SET status='queued',execution=NULL,grant_hash=NULL,grant_ciphertext=NULL WHERE id=$1").bind(id).execute(&mut *tx).await?;
            }
            tx.commit().await?;
        }
        Ok(())
    }

    pub async fn deliveries(&self) -> Result<Vec<OutboxDelivery>> {
        let rows = sqlx::query(
            "SELECT * FROM runtime_outbox WHERE NOT delivered ORDER BY created_at LIMIT 100",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| OutboxDelivery {
                id: row.get("id"),
                run_id: row.get("run_id"),
                kind: row.get("kind"),
                payload: row.get("payload"),
            })
            .collect())
    }

    pub async fn acknowledge_delivery(
        &self,
        event: Uuid,
        run: Uuid,
        kind: &str,
        payload: &Value,
        result: &Value,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let run_row = locked(&mut tx, run).await?;
        let already: bool =
            sqlx::query_scalar("SELECT delivered FROM runtime_outbox WHERE id=$1 FOR UPDATE")
                .bind(event)
                .fetch_one(&mut *tx)
                .await?;
        if already {
            return Ok(());
        }
        if kind == "proposal" {
            let mut life: Lifecycle = serde_json::from_value(run_row.get("lifecycle"))?;
            life.pending = false;
            save(&mut tx, run, &life).await?;
            sqlx::query("UPDATE runtime_proposals SET result=$2 WHERE id=$1 AND result IS NULL")
                .bind(id(payload, "proposal_id")?)
                .bind(result)
                .execute(&mut *tx)
                .await?;
            if result["accepted"] == true {
                sqlx::query("UPDATE runtime_runs SET status=$2 WHERE id=$1 AND status<>'canceled'")
                    .bind(run)
                    .bind(if payload["kind"] == "request_input" {
                        "needs_input"
                    } else {
                        "succeeded"
                    })
                    .execute(&mut *tx)
                    .await?;
            }
        }
        sqlx::query("UPDATE runtime_outbox SET delivered=true WHERE id=$1")
            .bind(event)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn reserve_model(&self, request: ModelRequest<'_>) -> Result<ModelReservation> {
        let ModelRequest {
            run,
            generation,
            lease,
            request_id,
            path,
            value,
            hash,
        } = request;
        let mut tx = self.pool.begin().await?;
        let row = locked(&mut tx, run).await?;
        let mut life = authorized(&row, lease)?;
        let cfg = config(self, &row)?.model;
        let expected = match cfg.api.as_str() {
            "openai-completions" => "v1/chat/completions",
            "openai-responses" => "v1/responses",
            "anthropic-messages" => "v1/messages",
            _ => "",
        };
        ensure!(
            path == expected && value["model"] == cfg.model,
            "model endpoint or identity mismatch"
        );
        ensure!(
            !value
                .as_object()
                .is_some_and(|o| o.contains_key("base_url") || o.contains_key("api_key")),
            "invalid model request"
        );
        if let Some(prior) = sqlx::query(
            "SELECT input_hash FROM runtime_model_requests WHERE run_id=$1 AND request_id=$2",
        )
        .bind(run)
        .bind(request_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            ensure!(
                prior.get::<String, _>("input_hash") == hash,
                "model request ID reused"
            );
            return Ok(ModelReservation::Duplicate);
        }
        life.reserve(generation, &lease.uid, Utc::now().timestamp_millis())?;
        sqlx::query("INSERT INTO runtime_profiles(id) VALUES($1) ON CONFLICT DO NOTHING")
            .bind(cfg.id)
            .execute(&mut *tx)
            .await?;
        let rpm = cfg.parameters["requests_per_minute"].as_u64().unwrap_or(0);
        let wait: i64 = if rpm > 0 {
            sqlx::query_scalar("UPDATE runtime_profiles SET next_call_at=greatest(next_call_at,clock_timestamp())+make_interval(secs=>$2) WHERE id=$1 RETURNING greatest(0,ceil(extract(epoch FROM next_call_at-clock_timestamp())*1000-$2*1000)::bigint)").bind(cfg.id).bind(60.0/rpm as f64+1.0).fetch_one(&mut *tx).await?
        } else {
            0
        };
        ensure!(
            Utc::now().timestamp_millis() + wait < life.deadline_ms.unwrap(),
            "profile queue exceeds deadline"
        );
        sqlx::query("INSERT INTO runtime_model_requests(run_id,request_id,input_hash,generation) VALUES($1,$2,$3,$4)").bind(run).bind(request_id).bind(hash).bind(generation).execute(&mut *tx).await?;
        save(&mut tx, run, &life).await?;
        tx.commit().await?;
        Ok(ModelReservation::Reserved {
            config: cfg,
            deadline_ms: life.deadline_ms.unwrap(),
            wait_ms: wait,
            endpoint: expected.into(),
        })
    }

    pub async fn model_response_received(&self, run: Uuid, request_id: Uuid) -> Result<()> {
        sqlx::query(
            "UPDATE runtime_model_requests SET state='response' WHERE run_id=$1 AND request_id=$2",
        )
        .bind(run)
        .bind(request_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
pub fn database_failure_category(error: &anyhow::Error) -> Option<&'static str> {
    error
        .downcast_ref::<sqlx::Error>()
        .map(|error| match error {
            sqlx::Error::PoolTimedOut => "database_pool_timeout",
            sqlx::Error::Database(_) => "database_statement",
            _ => "database_transport",
        })
}

impl RuntimeStore {
    pub async fn connect(url: &str, key: Arc<[u8; 32]>) -> Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(24)
            .connect(url)
            .await?;
        artifactory_database::require_timescale(&pool).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self::new(pool, key))
    }
}
