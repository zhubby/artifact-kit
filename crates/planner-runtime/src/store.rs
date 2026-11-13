//! Runtime persistence. Lifecycle rules execute with the locked persistent state.
mod models;
use crate::{lifecycle::Lifecycle, server::id};
use anyhow::{Context, Result, ensure};
use artifactory_database::{params, sql};
use artifactory_runtime_protocol::{Execution, Lease, RunIntent, crypto};
use chrono::Utc;
use serde_json::{Value, json};
use toasty::stmt::Type;

use std::sync::Arc;
use uuid::Uuid;
#[derive(Clone)]
pub struct RuntimeStore {
    pool: toasty::Db,
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

async fn locked(tx: &mut toasty::db::Transaction<'_>, run: Uuid) -> Result<sql::Record> {
    Ok(sql::record(
        tx,
        "SELECT * FROM runtime_runs WHERE id=$1 FOR UPDATE",
        params![run],
        &[
            ("id", Type::Uuid),
            ("version", Type::I64),
            ("intent_hash", Type::String),
            ("intent_ciphertext", Type::String),
            ("project_id", Type::Uuid),
            ("pool", Type::String),
            ("snapshot_id", Type::Uuid),
            ("snapshot_digest", Type::String),
            ("lifecycle", Type::String),
            ("status", Type::String),
            ("execution", Type::String),
            ("grant_hash", Type::String),
            ("grant_ciphertext", Type::String),
            ("checkpoint", Type::String),
            ("usage", Type::String),
            ("report_sequence", Type::I64),
            ("created_at", Type::Timestamp),
            ("dispatched_at", Type::Timestamp),
            ("updated_at", Type::Timestamp),
        ],
    )
    .await?)
}
fn config(s: &RuntimeStore, row: &sql::Record) -> Result<RunIntent> {
    Ok(serde_json::from_slice(&crypto::open(
        &s.key,
        &row.decode::<String>("intent_ciphertext")?,
    )?)?)
}
fn authorized(row: &sql::Record, lease: &Lease) -> Result<Lifecycle> {
    ensure!(
        row.decode::<Option<String>>("grant_hash")?.as_deref() == Some(&crypto::hash(&lease.token)),
        "invalid run grant"
    );
    let life: Lifecycle = serde_json::from_value(row.decode("lifecycle")?)?;
    life.valid(lease.generation, &lease.uid, Utc::now().timestamp_millis())?;
    ensure!(
        !["succeeded", "needs_input", "failed", "canceled"]
            .contains(&row.decode::<String>("status")?.as_str()),
        "run is terminal"
    );
    Ok(life)
}
async fn save(tx: &mut toasty::db::Transaction<'_>, run: Uuid, life: &Lifecycle) -> Result<()> {
    sql::execute(
        tx,
        "UPDATE runtime_runs SET lifecycle=$2,updated_at=now() WHERE id=$1",
        params![run, serde_json::to_value(life)?],
    )
    .await?;
    Ok(())
}
async fn enqueue(
    tx: &mut toasty::db::Transaction<'_>,
    run: Uuid,
    kind: &str,
    payload: Value,
) -> Result<Uuid> {
    let event = Uuid::new_v4();
    models::Outbox::create()
        .id(event)
        .run_id(run)
        .kind(kind)
        .payload(toasty::Json(payload))
        .exec(tx)
        .await?;
    Ok(event)
}
impl RuntimeStore {
    pub fn new(pool: toasty::Db, key: Arc<[u8; 32]>) -> Self {
        Self { pool, key }
    }
    pub async fn metrics(&self) -> Result<RuntimeMetrics> {
        let rows = sql::records(
            &mut self.pool.clone(),
            "SELECT status,count(*) AS runs FROM runtime_runs GROUP BY status",
            params![],
            &[("status", Type::String), ("runs", Type::I64)],
        )
        .await?;
        let (calls,pending,oldest):(i64,i64,f64)=sql::tuple(&mut self.pool.clone(), "SELECT coalesce(sum((lifecycle->>'calls')::bigint),0)::bigint,count(*) FILTER(WHERE (lifecycle->>'pending')::bool),coalesce(max(extract(epoch FROM now()-created_at)) FILTER(WHERE status='queued'),0)::float8 FROM runtime_runs", params![]).await?;
        let outbox: i64 = sql::scalar(
            &mut self.pool.clone(),
            "SELECT count(*) FROM runtime_outbox WHERE NOT delivered",
            params![],
        )
        .await?;
        Ok(RuntimeMetrics {
            statuses: rows
                .into_iter()
                .map(|row| Ok((row.decode("status")?, row.decode("runs")?)))
                .collect::<artifactory_database::Result<_>>()?,
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
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        sql::execute(
            &mut tx,
            "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
            params![run.to_string()],
        )
        .await?;
        if let Some(row) = sql::optional_record(
            &mut tx,
            "SELECT version,intent_hash,status FROM runtime_runs WHERE id=$1 FOR UPDATE",
            params![run],
            &[
                ("version", Type::I64),
                ("intent_hash", Type::String),
                ("status", Type::String),
            ],
        )
        .await?
        {
            if row.decode::<i64>("version")? > version {
                return Ok(json!({"ok":true,"stale":true}));
            }
            if row.decode::<i64>("version")? == version {
                ensure!(
                    row.decode::<Option<String>>("intent_hash")?.as_deref() == Some(&hash),
                    "intent ID/version reused with different input"
                );
                return Ok(json!({"ok":true}));
            }
            ensure!(cancel, "running intent is immutable");
            sql::execute(&mut tx, "UPDATE runtime_runs SET version=$2,intent_hash=$3,status='canceled',lifecycle=jsonb_set(lifecycle,'{canceled}','true'),updated_at=now() WHERE id=$1", params![run, version, hash]).await?;
        } else {
            let mut life = Lifecycle::new(intent.as_ref().map_or(20, |i| i.model.max_calls));
            life.canceled = cancel;
            models::Run::create()
                .id(run)
                .version(version)
                .intent_hash(hash)
                .intent_ciphertext(ciphertext)
                .project_id(intent.as_ref().map(|i| i.project_id))
                .pool(intent.as_ref().map(|i| i.pool.clone()))
                .snapshot_id(intent.as_ref().map(|i| i.snapshot_id))
                .snapshot_digest(intent.as_ref().map(|i| i.snapshot_digest.clone()))
                .lifecycle(toasty::Json(serde_json::to_value(life)?))
                .status(if cancel { "canceled" } else { "queued" })
                .exec(&mut tx)
                .await?;
        }
        tx.commit().await?;
        Ok(json!({"ok":true}))
    }

    pub async fn register_execution(&self, execution: Execution) -> Result<Value> {
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        let row = locked(&mut tx, execution.run_id).await?;
        let life: Lifecycle = serde_json::from_value(row.decode("lifecycle")?)?;
        ensure!(
            !life.canceled && life.generation == execution.generation,
            "execution fenced"
        );
        let existing: Option<Value> = row.decode("execution")?;
        if let Some(existing) = existing {
            ensure!(
                existing == serde_json::to_value(&execution)?,
                "execution identity already registered"
            );
        } else {
            sql::execute(
                &mut tx,
                "UPDATE runtime_runs SET execution=$2 WHERE id=$1",
                params![execution.run_id, serde_json::to_value(execution)?],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(json!({"ok":true}))
    }

    pub async fn heartbeat_run(&self, lease: Lease) -> Result<Value> {
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        let row = locked(&mut tx, lease.run_id).await?;
        let mut life = authorized(&row, &lease)?;
        life.lease_until_ms =
            Some((Utc::now().timestamp_millis() + 90_000).min(life.deadline_ms.unwrap()));
        save(&mut tx, lease.run_id, &life).await?;
        tx.commit().await?;
        Ok(json!({"ok":true}))
    }

    pub async fn validate_grant(&self, lease: Lease) -> Result<Value> {
        let row = sql::record(
            &mut self.pool.clone(),
            "SELECT * FROM runtime_runs WHERE id=$1",
            params![lease.run_id],
            &[
                ("id", Type::Uuid),
                ("version", Type::I64),
                ("intent_hash", Type::String),
                ("intent_ciphertext", Type::String),
                ("project_id", Type::Uuid),
                ("pool", Type::String),
                ("snapshot_id", Type::Uuid),
                ("snapshot_digest", Type::String),
                ("lifecycle", Type::String),
                ("status", Type::String),
                ("execution", Type::String),
                ("grant_hash", Type::String),
                ("grant_ciphertext", Type::String),
                ("checkpoint", Type::String),
                ("usage", Type::String),
                ("report_sequence", Type::I64),
                ("created_at", Type::Timestamp),
                ("dispatched_at", Type::Timestamp),
                ("updated_at", Type::Timestamp),
            ],
        )
        .await?;
        let life = authorized(&row, &lease)?;
        ensure!(!life.pending, "proposal awaiting acceptance");
        Ok(
            json!({"snapshot_id":row.decode::<Uuid>("snapshot_id")?,"project_id":row.decode::<Uuid>("project_id")?}),
        )
    }

    pub async fn read_run(&self, run: Uuid) -> Result<Value> {
        let row = sql::record(
            &mut self.pool.clone(),
            "SELECT * FROM runtime_runs WHERE id=$1",
            params![run],
            &[
                ("id", Type::Uuid),
                ("version", Type::I64),
                ("intent_hash", Type::String),
                ("intent_ciphertext", Type::String),
                ("project_id", Type::Uuid),
                ("pool", Type::String),
                ("snapshot_id", Type::Uuid),
                ("snapshot_digest", Type::String),
                ("lifecycle", Type::String),
                ("status", Type::String),
                ("execution", Type::String),
                ("grant_hash", Type::String),
                ("grant_ciphertext", Type::String),
                ("checkpoint", Type::String),
                ("usage", Type::String),
                ("report_sequence", Type::I64),
                ("created_at", Type::Timestamp),
                ("dispatched_at", Type::Timestamp),
                ("updated_at", Type::Timestamp),
            ],
        )
        .await?;
        Ok(
            json!({"run_id":run,"status":row.decode::<String>("status")?,"lifecycle":row.decode::<Value>("lifecycle")?,"execution":row.decode::<Option<Value>>("execution")?}),
        )
    }

    pub async fn report_event(&self, v: Value) -> Result<Value> {
        let lease: Lease = serde_json::from_value(v["lease"].clone())?;
        let event = id(&v, "event_id")?;
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        let row = locked(&mut tx, lease.run_id).await?;
        authorized(&row, &lease)?;
        let hash = crypto::hash(serde_json::to_vec(&v)?);
        let prior: Option<String> = sql::optional_scalar(
            &mut tx,
            "SELECT input_hash FROM runtime_reports WHERE run_id=$1 AND event_id=$2",
            params![lease.run_id, event],
        )
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
        if sequence > row.decode::<i64>("report_sequence")? {
            if kind == "checkpoint" {
                sql::execute(
                    &mut tx,
                    "UPDATE runtime_runs SET checkpoint=$2,report_sequence=$3 WHERE id=$1",
                    params![lease.run_id, &v["payload"], sequence],
                )
                .await?;
            }
            if kind == "usage" {
                sql::execute(
                    &mut tx,
                    "UPDATE runtime_runs SET usage=$2,report_sequence=$3 WHERE id=$1",
                    params![lease.run_id, &v["payload"], sequence],
                )
                .await?;
            }
            if kind == "failed" {
                sql::execute(
                    &mut tx,
                    "UPDATE runtime_runs SET status='failed' WHERE id=$1",
                    params![lease.run_id],
                )
                .await?;
                enqueue(&mut tx,lease.run_id,"failed",json!({"error":"Pi planning failed","calls":serde_json::from_value::<Lifecycle>(row.decode("lifecycle")?)?.calls})).await?;
            }
        }
        models::Report::create()
            .run_id(lease.run_id)
            .event_id(event)
            .input_hash(hash)
            .exec(&mut tx)
            .await?;
        tx.commit().await?;
        Ok(json!({"ok":true}))
    }

    pub async fn acquire_execution(&self, execution: Execution, public_url: &str) -> Result<Value> {
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        let row = locked(&mut tx, execution.run_id).await?;
        ensure!(
            matches!(
                row.decode::<String>("status")?.as_str(),
                "provisioning" | "running"
            ),
            "run is not admitted for execution"
        );
        ensure!(
            row.decode::<Option<Value>>("execution")? == Some(serde_json::to_value(&execution)?),
            "unregistered Pod UID"
        );
        let mut life: Lifecycle = serde_json::from_value(row.decode("lifecycle")?)?;
        let intent = config(self, &row)?;
        life.acquire(
            execution.generation,
            &execution.uid,
            Utc::now().timestamp_millis(),
            i64::from(intent.model.timeout_seconds) * 1000,
        )?;
        let token = if let Some(ciphertext) = row.decode::<Option<String>>("grant_ciphertext")? {
            String::from_utf8(crypto::open(&self.key, &ciphertext)?)?
        } else {
            crypto::token()
        };
        sql::execute(&mut tx, "UPDATE runtime_runs SET status='running',lifecycle=$2,grant_hash=$3,grant_ciphertext=$4 WHERE id=$1", params![execution.run_id, serde_json::to_value(&life)?, crypto::hash(&token), crypto::seal(&self.key,token.as_bytes())?]).await?;
        let source_url = intent.context.get("source_url").cloned();
        let mut context = intent.context;
        context["checkpoint"] = row
            .decode::<Option<Value>>("checkpoint")?
            .unwrap_or(Value::Null);
        context["usage"] = row.decode::<Value>("usage")?;
        let rejected:Option<Value>=sql::optional_scalar(&mut tx, "SELECT jsonb_build_object('kind',kind,'payload',payload,'result',result) FROM runtime_proposals WHERE run_id=$1 AND result IS NOT NULL ORDER BY created_at DESC LIMIT 1", params![execution.run_id]).await?;
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
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        let row = locked(&mut tx, lease.run_id).await?;
        let prior = sql::optional_record(
            &mut tx,
            "SELECT input_hash,result FROM runtime_proposals WHERE id=$1 AND run_id=$2",
            params![proposal, lease.run_id],
            &[("input_hash", Type::String), ("result", Type::String)],
        )
        .await?;
        let hash = crypto::hash(serde_json::to_vec(
            &json!({"name":name,"arguments":v["arguments"]}),
        )?);
        if let Some(prior) = prior {
            ensure!(
                prior.decode::<String>("input_hash")? == hash,
                "proposal ID reused"
            );
            return Ok(ToolDisposition::Immediate(
                prior
                    .decode::<Option<Value>>("result")?
                    .unwrap_or(json!({"pending":true,"proposal_id":proposal})),
            ));
        }
        let mut life = authorized(&row, &lease)?;
        ensure!(!life.pending, "proposal awaiting acceptance");
        if matches!(name, "submit_plan" | "request_input") {
            life.pending = true;
            models::Proposal::create()
                .id(proposal)
                .run_id(lease.run_id)
                .generation(lease.generation)
                .input_hash(hash)
                .kind(name)
                .payload(toasty::Json(v["arguments"].clone()))
                .deadline_ms(life.deadline_ms.unwrap())
                .exec(&mut tx)
                .await?;
            save(&mut tx, lease.run_id, &life).await?;
            enqueue(&mut tx,lease.run_id,"proposal",json!({"proposal_id":proposal,"generation":lease.generation,"kind":name,"payload":v["arguments"],"deadline_ms":life.deadline_ms,"snapshot_digest":row.decode::<String>("snapshot_digest")?})).await?;
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
        let row=sql::record(&mut self.pool.clone(), "SELECT p.result FROM runtime_proposals p JOIN runtime_runs r ON r.id=p.run_id WHERE p.id=$1 AND p.run_id=$2 AND r.grant_hash=$3 AND p.generation=$4", params![proposal, lease.run_id, crypto::hash(lease.token), lease.generation], &[("result", Type::String)]).await?;
        Ok(row
            .decode::<Option<Value>>("result")?
            .unwrap_or(json!({"pending":true})))
    }

    pub async fn queue(&self, pool: &str, limit: i64, per_project: i64) -> Result<Value> {
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        sql::execute(
            &mut tx,
            "SELECT pg_advisory_xact_lock(hashtextextended($1,1))",
            params![pool],
        )
        .await?;
        let active: i64 = sql::scalar(&mut tx, "SELECT count(*) FROM runtime_runs WHERE pool=$1 AND status IN ('provisioning','running')", params![pool])
    .await?;
        let rows=sql::records(&mut tx, "SELECT * FROM runtime_runs r WHERE pool=$1 AND status='queued' AND (SELECT count(*) FROM runtime_runs a WHERE a.project_id=r.project_id AND a.pool=r.pool AND a.status IN ('provisioning','running'))<$3 ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT $2", params![pool, (limit-active).max(0), per_project], &[("id", Type::Uuid),("version", Type::I64),("intent_hash", Type::String),("intent_ciphertext", Type::String),("project_id", Type::Uuid),("pool", Type::String),("snapshot_id", Type::Uuid),("snapshot_digest", Type::String),("lifecycle", Type::String),("status", Type::String),("execution", Type::String),("grant_hash", Type::String),("grant_ciphertext", Type::String),("checkpoint", Type::String),("usage", Type::String),("report_sequence", Type::I64),("created_at", Type::Timestamp),("dispatched_at", Type::Timestamp),("updated_at", Type::Timestamp)]).await?;
        let mut admitted = Vec::new();
        for row in rows {
            let project: Uuid = row.decode("project_id")?;
            let count:i64=sql::scalar(&mut tx, "SELECT count(*) FROM runtime_runs WHERE project_id=$1 AND pool=$2 AND status IN ('provisioning','running')", params![project, pool]).await?;
            if count >= per_project {
                continue;
            }
            let run: Uuid = row.decode("id")?;
            let life: Lifecycle = serde_json::from_value(row.decode("lifecycle")?)?;
            sql::execute(
                &mut tx,
                "UPDATE runtime_runs SET status='provisioning',dispatched_at=now() WHERE id=$1",
                params![run],
            )
            .await?;
            admitted.push(json!({"run_id":run,"generation":life.generation,"pool":pool,"snapshot_id":row.decode::<Uuid>("snapshot_id")?,"snapshot_digest":row.decode::<String>("snapshot_digest")?}));
        }
        // Lost create acknowledgements are recovered by returning all admitted descriptors.
        let active = sql::records(
            &mut tx,
            "SELECT * FROM runtime_runs WHERE pool=$1 AND status IN ('provisioning','running')",
            params![pool],
            &[
                ("id", Type::Uuid),
                ("version", Type::I64),
                ("intent_hash", Type::String),
                ("intent_ciphertext", Type::String),
                ("project_id", Type::Uuid),
                ("pool", Type::String),
                ("snapshot_id", Type::Uuid),
                ("snapshot_digest", Type::String),
                ("lifecycle", Type::String),
                ("status", Type::String),
                ("execution", Type::String),
                ("grant_hash", Type::String),
                ("grant_ciphertext", Type::String),
                ("checkpoint", Type::String),
                ("usage", Type::String),
                ("report_sequence", Type::I64),
                ("created_at", Type::Timestamp),
                ("dispatched_at", Type::Timestamp),
                ("updated_at", Type::Timestamp),
            ],
        )
        .await?;
        let mut rows = Vec::new();
        for row in active {
            let intent = config(self, &row)?;
            let lifecycle: Lifecycle = serde_json::from_value(row.decode("lifecycle")?)?;
            rows.push(json!({"run_id":intent.run_id,"generation":lifecycle.generation,"pool":intent.pool,"snapshot_id":intent.snapshot_id,"snapshot_digest":intent.snapshot_digest,"source_url":intent.context["source_url"]}));
        }
        tx.commit().await?;
        let queued: i64 = sql::scalar(
            &mut self.pool.clone(),
            "SELECT count(*) FROM runtime_runs WHERE pool=$1 AND status='queued'",
            params![pool],
        )
        .await?;
        Ok(json!({"active":rows.len(),"queued":queued,"runs":rows}))
    }

    pub async fn maintain(&self) -> Result<()> {
        let ids: Vec<Uuid> = sql::scalars(
            &mut self.pool.clone(),
            "SELECT id FROM runtime_runs WHERE status IN ('provisioning','running')",
            params![],
        )
        .await?;
        for id in ids {
            let mut tx_db = self.pool.clone();
            let mut tx = tx_db.transaction().await?;
            let row = locked(&mut tx, id).await?;
            let mut life: Lifecycle = serde_json::from_value(row.decode("lifecycle")?)?;
            let now = Utc::now().timestamp_millis();
            if !matches!(
                row.decode::<String>("status")?.as_str(),
                "provisioning" | "running"
            ) {
                continue;
            }
            if life.pending {
                continue;
            }
            let expired = life.deadline_ms.is_some_and(|d| d <= now) || life.generation >= 4;
            let stalled = row.decode::<String>("status")? == "provisioning"
                && row
                    .decode::<Option<chrono::DateTime<Utc>>>("dispatched_at")?
                    .is_some_and(|t| t < Utc::now() - chrono::Duration::seconds(180));
            if expired || (stalled && life.generation >= 3) {
                sql::execute(
                    &mut tx,
                    "UPDATE runtime_runs SET status='failed' WHERE id=$1",
                    params![id],
                )
                .await?;
                enqueue(&mut tx,id,"failed",json!({"error":"planning deadline or provisioning retry limit reached","calls":life.calls})).await?;
            } else if stalled || life.lease_until_ms.is_some_and(|until| until <= now) {
                life.retry(now)?;
                save(&mut tx, id, &life).await?;
                sql::execute(&mut tx, "UPDATE runtime_runs SET status='queued',execution=NULL,grant_hash=NULL,grant_ciphertext=NULL WHERE id=$1", params![id]).await?;
            }
            tx.commit().await?;
        }
        Ok(())
    }

    pub async fn deliveries(&self) -> Result<Vec<OutboxDelivery>> {
        let rows = sql::records(
            &mut self.pool.clone(),
            "SELECT * FROM runtime_outbox WHERE NOT delivered ORDER BY created_at LIMIT 100",
            params![],
            &[
                ("id", Type::Uuid),
                ("run_id", Type::Uuid),
                ("kind", Type::String),
                ("payload", Type::String),
                ("delivered", Type::Bool),
                ("created_at", Type::Timestamp),
            ],
        )
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                Ok(OutboxDelivery {
                    id: row.decode("id")?,
                    run_id: row.decode("run_id")?,
                    kind: row.decode("kind")?,
                    payload: row.decode("payload")?,
                })
            })
            .collect::<artifactory_database::Result<_>>()?)
    }

    pub async fn acknowledge_delivery(
        &self,
        event: Uuid,
        run: Uuid,
        kind: &str,
        payload: &Value,
        result: &Value,
    ) -> Result<()> {
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        let run_row = locked(&mut tx, run).await?;
        let already: bool = sql::scalar(
            &mut tx,
            "SELECT delivered FROM runtime_outbox WHERE id=$1 FOR UPDATE",
            params![event],
        )
        .await?;
        if already {
            return Ok(());
        }
        if kind == "proposal" {
            let mut life: Lifecycle = serde_json::from_value(run_row.decode("lifecycle")?)?;
            life.pending = false;
            save(&mut tx, run, &life).await?;
            sql::execute(
                &mut tx,
                "UPDATE runtime_proposals SET result=$2 WHERE id=$1 AND result IS NULL",
                params![id(payload, "proposal_id")?, result],
            )
            .await?;
            if result["accepted"] == true {
                sql::execute(
                    &mut tx,
                    "UPDATE runtime_runs SET status=$2 WHERE id=$1 AND status<>'canceled'",
                    params![
                        run,
                        if payload["kind"] == "request_input" {
                            "needs_input"
                        } else {
                            "succeeded"
                        }
                    ],
                )
                .await?;
            }
        }
        models::Outbox::filter_by_id(event)
            .update()
            .delivered(true)
            .exec(&mut tx)
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
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
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
        if let Some(prior) = sql::optional_record(
            &mut tx,
            "SELECT input_hash FROM runtime_model_requests WHERE run_id=$1 AND request_id=$2",
            params![run, request_id],
            &[("input_hash", Type::String)],
        )
        .await?
        {
            ensure!(
                prior.decode::<String>("input_hash")? == hash,
                "model request ID reused"
            );
            return Ok(ModelReservation::Duplicate);
        }
        life.reserve(generation, &lease.uid, Utc::now().timestamp_millis())?;
        models::Profile::upsert_by_id(cfg.id)
            .or_ignore()
            .exec(&mut tx)
            .await?;
        let rpm = cfg.parameters["requests_per_minute"].as_u64().unwrap_or(0);
        let wait: i64 = if rpm > 0 {
            sql::scalar(&mut tx, "UPDATE runtime_profiles SET next_call_at=greatest(next_call_at,clock_timestamp())+make_interval(secs=>$2) WHERE id=$1 RETURNING greatest(0,ceil(extract(epoch FROM next_call_at-clock_timestamp())*1000-$2*1000)::bigint)", params![cfg.id, 60.0/rpm as f64+1.0]).await?
        } else {
            0
        };
        ensure!(
            Utc::now().timestamp_millis() + wait < life.deadline_ms.unwrap(),
            "profile queue exceeds deadline"
        );
        models::ModelRequest::create()
            .run_id(run)
            .request_id(request_id)
            .input_hash(hash)
            .generation(generation)
            .exec(&mut tx)
            .await?;
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
        sql::execute(
            &mut self.pool.clone(),
            "UPDATE runtime_model_requests SET state='response' WHERE run_id=$1 AND request_id=$2",
            params![run, request_id],
        )
        .await?;
        Ok(())
    }
}
pub fn database_failure_category(error: &anyhow::Error) -> Option<&'static str> {
    let backend = error
        .downcast_ref::<artifactory_database::Error>()
        .and_then(|error| match error {
            artifactory_database::Error::Backend(error) => Some(error),
            _ => None,
        })
        .or_else(|| error.downcast_ref::<toasty::Error>())?;
    Some(if backend.is_connection_pool() {
        "database_pool_timeout"
    } else if backend.is_driver_operation_failed() {
        "database_statement"
    } else {
        "database_transport"
    })
}

impl RuntimeStore {
    pub async fn connect(url: &str, key: Arc<[u8; 32]>) -> Result<Self> {
        let pool = artifactory_database::connect(
            url,
            models(),
            artifactory_database::PoolSettings::new(24),
        )
        .await?;
        artifactory_database::require_timescale(&pool).await?;
        artifactory_database::migrate(&pool, &MIGRATIONS).await?;
        Ok(Self::new(pool, key))
    }
}

pub static MIGRATIONS: toasty::migration::MigrationSet = toasty::embed_migrations!("migrations");

pub fn models() -> toasty::schema::app::ModelSet {
    toasty::models!(crate::*)
}
