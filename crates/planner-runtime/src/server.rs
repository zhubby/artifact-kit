use crate::lifecycle::Lifecycle;
use anyhow::{Context, Result, ensure};
use artifactory_runtime_protocol::{
    Execution, Lease, RunIntent, crypto,
    service::{self, Client},
};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::StatusCode,
    response::Response,
    routing::{get, post},
};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::{Row, postgres::PgRow};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;
type ApiResult<T> = artifactory_runtime_protocol::error::Result<Json<T>>;
#[derive(Clone)]
pub struct Runtime {
    pub pool: sqlx::PgPool,
    pub key: Arc<[u8; 32]>,
    pub control: Client,
    pub model_http: reqwest::Client,
    pub public_url: String,
}
pub async fn from_env() -> Result<Runtime> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(24)
        .connect(&service::required("DATABASE_URL")?)
        .await?;
    artifactory_database::require_timescale(&pool).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(Runtime {
        pool,
        key: Arc::new(service::key("ARTIFACTORY_SERVICE_KEY")?),
        control: Client::new(
            service::required("ARTIFACTORY_CONTROL_URL")?,
            service::required("ARTIFACTORY_SERVICE_TOKEN")?,
        )?,
        model_http: service::client()?,
        public_url: service::required("ARTIFACTORY_RUNTIME_URL")?,
    })
}
pub fn router(state: Runtime, token: String) -> Router {
    let internal = Router::new()
        .route("/metrics", get(metrics))
        .route("/v1/intent", post(intent))
        .route("/v1/register", post(register))
        .route("/v1/get", post(get_run))
        .route("/v1/validate", post(validate))
        .route("/v1/queue", post(queue))
        .route("/v1/reconcile", post(reconcile))
        .with_state(state.clone());
    let runner = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/acquire", post(acquire))
        .route("/v1/heartbeat", post(heartbeat))
        .route("/v1/report", post(report))
        .route("/v1/tool", post(tool))
        .route("/v1/proposal", post(proposal_result))
        .route("/model/{run}/{generation}/{uid}/{*path}", post(model))
        .with_state(state);
    runner
        .merge(service::protected(internal, token))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
}
async fn metrics(State(s): State<Runtime>) -> artifactory_runtime_protocol::error::Result<String> {
    let mut output = artifactory_runtime_protocol::metrics::render();
    for row in sqlx::query("SELECT status,count(*) AS runs FROM runtime_runs GROUP BY status")
        .fetch_all(&s.pool)
        .await?
    {
        output.push_str(&format!(
            "artifactory_runtime_runs{{status=\"{}\"}} {}\n",
            row.get::<String, _>("status"),
            row.get::<i64, _>("runs")
        ));
    }
    let (calls,pending,oldest):(i64,i64,f64)=sqlx::query_as("SELECT coalesce(sum((lifecycle->>'calls')::bigint),0)::bigint,count(*) FILTER(WHERE (lifecycle->>'pending')::bool),coalesce(max(extract(epoch FROM now()-created_at)) FILTER(WHERE status='queued'),0)::float8 FROM runtime_runs").fetch_one(&s.pool).await?;
    output.push_str(&format!("artifactory_runtime_model_reservations_total {calls}\nartifactory_runtime_pending_proposals {pending}\nartifactory_runtime_oldest_queued_seconds {oldest}\n"));
    let outbox: i64 = sqlx::query_scalar("SELECT count(*) FROM runtime_outbox WHERE NOT delivered")
        .fetch_one(&s.pool)
        .await?;
    output.push_str(&format!("artifactory_runtime_outbox_pending {outbox}\n"));
    Ok(output)
}
pub fn id(v: &Value, key: &str) -> Result<Uuid> {
    Ok(Uuid::parse_str(
        v[key].as_str().context("missing identifier")?,
    )?)
}
pub async fn put_intent(state: &Runtime, value: Value) -> Result<Value> {
    let run = id(&value, "run_id")?;
    let version = value["version"]
        .as_i64()
        .context("missing intent version")?;
    ensure!(version > 0, "invalid version");
    let cancel = value["canceled"].as_bool() == Some(true);
    let hash = crypto::hash(serde_json::to_vec(&value)?);
    let intent = if cancel {
        None
    } else {
        Some(serde_json::from_value::<RunIntent>(value.clone())?)
    };
    if let Some(intent) = &intent {
        ensure!(
            (1..=100).contains(&intent.model.max_calls)
                && (30..=3600).contains(&intent.model.timeout_seconds),
            "invalid run budget"
        );
        service::endpoint(&intent.model.base_url)?;
        ensure!(
            matches!(
                intent.model.api.as_str(),
                "openai-completions" | "openai-responses" | "anthropic-messages"
            ),
            "unsupported model protocol"
        );
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(run.to_string())
        .execute(&mut *tx)
        .await?;
    if let Some(row) =
        sqlx::query("SELECT version,intent_hash,status FROM runtime_runs WHERE id=$1 FOR UPDATE")
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
   .bind(run).bind(version).bind(hash).bind(if cancel{None}else{Some(crypto::seal(&state.key,&serde_json::to_vec(&value)?)?)}).bind(intent.as_ref().map(|i|i.project_id)).bind(intent.as_ref().map(|i|&i.pool)).bind(intent.as_ref().map(|i|i.snapshot_id)).bind(intent.as_ref().map(|i|&i.snapshot_digest)).bind(serde_json::to_value(life)?).bind(if cancel{"canceled"}else{"queued"}).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(json!({"ok":true}))
}
async fn intent(State(s): State<Runtime>, Json(v): Json<Value>) -> ApiResult<Value> {
    Ok(Json(put_intent(&s, v).await?))
}
pub async fn register_execution(s: &Runtime, execution: Execution) -> Result<Value> {
    let mut tx = s.pool.begin().await?;
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
async fn register(State(s): State<Runtime>, Json(v): Json<Execution>) -> ApiResult<Value> {
    Ok(Json(register_execution(&s, v).await?))
}
async fn locked(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, run: Uuid) -> Result<PgRow> {
    Ok(
        sqlx::query("SELECT * FROM runtime_runs WHERE id=$1 FOR UPDATE")
            .bind(run)
            .fetch_one(&mut **tx)
            .await?,
    )
}
fn config(s: &Runtime, row: &PgRow) -> Result<RunIntent> {
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
async fn token_review(execution: &Execution, token: &str) -> Result<()> {
    // Bound service-account tokens are validated by Kubernetes, never by trusting decoded JWT claims.
    let host = service::required("KUBERNETES_SERVICE_HOST")?;
    let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or("443".into());
    let root = "/var/run/secrets/kubernetes.io/serviceaccount";
    let ca = reqwest::Certificate::from_pem(&tokio::fs::read(format!("{root}/ca.crt")).await?)?;
    let own = tokio::fs::read_to_string(format!("{root}/token")).await?;
    let http = reqwest::Client::builder()
        .add_root_certificate(ca)
        .build()?;
    let value:Value=http.post(format!("https://{host}:{port}/apis/authentication.k8s.io/v1/tokenreviews")).bearer_auth(own.trim()).json(&json!({"apiVersion":"authentication.k8s.io/v1","kind":"TokenReview","spec":{"token":token,"audiences":["artifactory-runtime"]}})).send().await?.error_for_status()?.json().await?;
    let status = &value["status"];
    ensure!(status["authenticated"] == true, "Pod token rejected");
    ensure!(
        status["user"]["username"]
            == format!(
                "system:serviceaccount:{}:artifactory-pi",
                execution.namespace
            ),
        "wrong runner service account"
    );
    ensure!(
        status["user"]["extra"]["authentication.kubernetes.io/pod-uid"][0] == execution.uid
            && status["user"]["extra"]["authentication.kubernetes.io/pod-name"][0]
                == execution.name,
        "Pod identity mismatch"
    );
    Ok(())
}
pub async fn acquire_execution(s: &Runtime, v: Value) -> Result<Value> {
    let execution: Execution = serde_json::from_value(v["execution"].clone())?;
    token_review(
        &execution,
        v["bootstrap_token"]
            .as_str()
            .context("Pod token required")?,
    )
    .await?;
    let mut tx = s.pool.begin().await?;
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
    let intent = config(s, &row)?;
    life.acquire(
        execution.generation,
        &execution.uid,
        Utc::now().timestamp_millis(),
        i64::from(intent.model.timeout_seconds) * 1000,
    )?;
    let token = if let Some(ciphertext) = row.get::<Option<String>, _>("grant_ciphertext") {
        String::from_utf8(crypto::open(&s.key, &ciphertext)?)?
    } else {
        crypto::token()
    };
    sqlx::query("UPDATE runtime_runs SET status='running',lifecycle=$2,grant_hash=$3,grant_ciphertext=$4 WHERE id=$1").bind(execution.run_id).bind(serde_json::to_value(&life)?).bind(crypto::hash(&token)).bind(crypto::seal(&s.key,token.as_bytes())?).execute(&mut *tx).await?;
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
        s.public_url, execution.run_id, execution.generation, execution.uid
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
async fn acquire(State(s): State<Runtime>, Json(v): Json<Value>) -> ApiResult<Value> {
    Ok(Json(acquire_execution(&s, v).await?))
}
pub async fn heartbeat_run(s: &Runtime, lease: Lease) -> Result<Value> {
    let mut tx = s.pool.begin().await?;
    let row = locked(&mut tx, lease.run_id).await?;
    let mut life = authorized(&row, &lease)?;
    life.lease_until_ms =
        Some((Utc::now().timestamp_millis() + 90_000).min(life.deadline_ms.unwrap()));
    save(&mut tx, lease.run_id, &life).await?;
    tx.commit().await?;
    Ok(json!({"ok":true}))
}
async fn heartbeat(State(s): State<Runtime>, Json(v): Json<Lease>) -> ApiResult<Value> {
    Ok(Json(heartbeat_run(&s, v).await?))
}
pub async fn validate_grant(s: &Runtime, lease: Lease) -> Result<Value> {
    let row = sqlx::query("SELECT * FROM runtime_runs WHERE id=$1")
        .bind(lease.run_id)
        .fetch_one(&s.pool)
        .await?;
    let life = authorized(&row, &lease)?;
    ensure!(!life.pending, "proposal awaiting acceptance");
    Ok(
        json!({"snapshot_id":row.get::<Uuid,_>("snapshot_id"),"project_id":row.get::<Uuid,_>("project_id")}),
    )
}
async fn validate(State(s): State<Runtime>, Json(v): Json<Lease>) -> ApiResult<Value> {
    Ok(Json(validate_grant(&s, v).await?))
}
pub async fn read_run(s: &Runtime, run: Uuid) -> Result<Value> {
    let row = sqlx::query("SELECT * FROM runtime_runs WHERE id=$1")
        .bind(run)
        .fetch_one(&s.pool)
        .await?;
    Ok(
        json!({"run_id":run,"status":row.get::<String,_>("status"),"lifecycle":row.get::<Value,_>("lifecycle"),"execution":row.get::<Option<Value>,_>("execution")}),
    )
}
async fn get_run(State(s): State<Runtime>, Json(v): Json<Value>) -> ApiResult<Value> {
    Ok(Json(read_run(&s, id(&v, "run_id")?).await?))
}
pub async fn report_event(s: &Runtime, v: Value) -> Result<Value> {
    let lease: Lease = serde_json::from_value(v["lease"].clone())?;
    let event = id(&v, "event_id")?;
    let mut tx = s.pool.begin().await?;
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
async fn report(State(s): State<Runtime>, Json(v): Json<Value>) -> ApiResult<Value> {
    Ok(Json(report_event(&s, v).await?))
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
pub async fn tool_call(s: &Runtime, v: Value) -> Result<Value> {
    let lease: Lease = serde_json::from_value(v["lease"].clone())?;
    let name = v["name"].as_str().context("tool name required")?;
    let proposal = id(&v, "request_id")?;
    let mut tx = s.pool.begin().await?;
    let row = locked(&mut tx, lease.run_id).await?;
    let prior =
        sqlx::query("SELECT input_hash,result FROM runtime_proposals WHERE id=$1 AND run_id=$2")
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
        return Ok(prior
            .get::<Option<Value>, _>("result")
            .unwrap_or(json!({"pending":true,"proposal_id":proposal})));
    }
    let mut life = authorized(&row, &lease)?;
    ensure!(!life.pending, "proposal awaiting acceptance");
    if matches!(name, "submit_plan" | "request_input") {
        life.pending = true;
        sqlx::query("INSERT INTO runtime_proposals(id,run_id,generation,input_hash,kind,payload,deadline_ms) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(proposal).bind(lease.run_id).bind(lease.generation).bind(hash).bind(name).bind(&v["arguments"]).bind(life.deadline_ms.unwrap()).execute(&mut *tx).await?;
        save(&mut tx, lease.run_id, &life).await?;
        enqueue(&mut tx,lease.run_id,"proposal",json!({"proposal_id":proposal,"generation":lease.generation,"kind":name,"payload":v["arguments"],"deadline_ms":life.deadline_ms,"snapshot_digest":row.get::<String,_>("snapshot_digest")})).await?;
        tx.commit().await?;
        Ok(json!({"pending":true,"proposal_id":proposal}))
    } else {
        ensure!(
            matches!(name, "node_capabilities" | "read_logs"),
            "source tools must use Source Service"
        );
        tx.commit().await?;
        s.control
            .post(
                "/internal/runtime/tool",
                &json!({"run_id":lease.run_id,"name":name,"arguments":v["arguments"]}),
            )
            .await
    }
}
async fn tool(State(s): State<Runtime>, Json(v): Json<Value>) -> ApiResult<Value> {
    Ok(Json(tool_call(&s, v).await?))
}
async fn proposal_result(State(s): State<Runtime>, Json(v): Json<Value>) -> ApiResult<Value> {
    let lease: Lease = serde_json::from_value(v["lease"].clone())?;
    // An expired owner may only query its already registered proposal; it cannot create a new one.
    let row=sqlx::query("SELECT p.result FROM runtime_proposals p JOIN runtime_runs r ON r.id=p.run_id WHERE p.id=$1 AND p.run_id=$2 AND r.grant_hash=$3 AND p.generation=$4").bind(id(&v,"proposal_id")?).bind(lease.run_id).bind(crypto::hash(lease.token)).bind(lease.generation).fetch_one(&s.pool).await?;
    Ok(Json(
        row.get::<Option<Value>, _>("result")
            .unwrap_or(json!({"pending":true})),
    ))
}
async fn queue(State(s): State<Runtime>, Json(v): Json<Value>) -> ApiResult<Value> {
    let pool = v["pool"].as_str().unwrap_or("default");
    let limit = v["limit"].as_i64().unwrap_or(8).clamp(0, 1000);
    let per_project = v["per_project"]
        .as_i64()
        .unwrap_or(4)
        .clamp(1, limit.max(1));
    let mut tx = s.pool.begin().await?;
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
        let intent = config(&s, &row)?;
        let lifecycle: Lifecycle = serde_json::from_value(row.get("lifecycle"))?;
        rows.push(json!({"run_id":intent.run_id,"generation":lifecycle.generation,"pool":intent.pool,"snapshot_id":intent.snapshot_id,"snapshot_digest":intent.snapshot_digest,"source_url":intent.context["source_url"]}));
    }
    tx.commit().await?;
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM runtime_runs WHERE pool=$1 AND status='queued'")
            .bind(pool)
            .fetch_one(&s.pool)
            .await?;
    Ok(Json(
        json!({"active":rows.len(),"queued":queued,"runs":rows}),
    ))
}
async fn reconcile(State(s): State<Runtime>, Json(_): Json<Value>) -> ApiResult<Value> {
    maintain(&s).await?;
    Ok(Json(json!({"ok":true})))
}
pub async fn maintain(s: &Runtime) -> Result<()> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM runtime_runs WHERE status IN ('provisioning','running')",
    )
    .fetch_all(&s.pool)
    .await?;
    for id in ids {
        let mut tx = s.pool.begin().await?;
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
    deliver(s).await
}
pub async fn deliver(s: &Runtime) -> Result<()> {
    let rows = sqlx::query(
        "SELECT * FROM runtime_outbox WHERE NOT delivered ORDER BY created_at LIMIT 100",
    )
    .fetch_all(&s.pool)
    .await?;
    for row in rows {
        let event: Uuid = row.get("id");
        let run: Uuid = row.get("run_id");
        let kind: String = row.get("kind");
        let payload: Value = row.get("payload");
        let result: Value = match s
            .control
            .post(
                "/internal/runtime/event",
                &json!({"event_id":event,"run_id":run,"kind":kind,"payload":payload}),
            )
            .await
        {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mut tx = s.pool.begin().await?;
        let run_row = locked(&mut tx, run).await?;
        let already: bool =
            sqlx::query_scalar("SELECT delivered FROM runtime_outbox WHERE id=$1 FOR UPDATE")
                .bind(event)
                .fetch_one(&mut *tx)
                .await?;
        if already {
            continue;
        }
        if kind == "proposal" {
            let mut life: Lifecycle = serde_json::from_value(run_row.get("lifecycle"))?;
            life.pending = false;
            save(&mut tx, run, &life).await?;
            sqlx::query("UPDATE runtime_proposals SET result=$2 WHERE id=$1 AND result IS NULL")
                .bind(id(&payload, "proposal_id")?)
                .bind(&result)
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
    }
    Ok(())
}
async fn model(
    State(s): State<Runtime>,
    Path((run, generation, uid, path)): Path<(Uuid, i64, String, String)>,
    request: Request,
) -> artifactory_runtime_protocol::error::Result<Response> {
    model_request(s, run, generation, uid, path, request)
        .await
        .map_err(|error| {
            // Do not log provider URLs, headers, request bodies or credentials.
            // Typed categories make failures diagnosable without exposing them.
            let category = if let Some(error) = error.downcast_ref::<reqwest::Error>() {
                if error.is_timeout() {
                    "upstream_timeout"
                } else if error.is_connect() {
                    "upstream_connect"
                } else {
                    "upstream_transport"
                }
            } else if let Some(error) = error.downcast_ref::<sqlx::Error>() {
                match error {
                    sqlx::Error::PoolTimedOut => "database_pool_timeout",
                    sqlx::Error::Database(_) => "database_statement",
                    _ => "database_transport",
                }
            } else {
                "run_validation"
            };
            tracing::warn!(%run, generation, category, "model proxy request failed");
            error.into()
        })
}
pub async fn model_request(
    s: Runtime,
    run: Uuid,
    generation: i64,
    uid: String,
    path: String,
    request: Request,
) -> Result<Response> {
    let started = std::time::Instant::now();
    let token = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| {
            request
                .headers()
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
        })
        .context("model grant required")?
        .to_string();
    let lease = Lease {
        run_id: run,
        generation,
        uid,
        token,
    };
    let request_id = request
        .headers()
        .get("x-artifactory-request-id")
        .and_then(|v| v.to_str().ok())
        .context("model request ID required")?
        .parse::<Uuid>()?;
    let body = axum::body::to_bytes(request.into_body(), 2 * 1024 * 1024).await?;
    let body_bytes = body.len();
    let value: Value = serde_json::from_slice(&body)?;
    let mut tx = s.pool.begin().await?;
    let row = locked(&mut tx, run).await?;
    let mut life = authorized(&row, &lease)?;
    let cfg = config(&s, &row)?.model;
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
    let hash = crypto::hash(&body);
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
        return Ok(Response::builder()
            .status(StatusCode::CONFLICT)
            .body(Body::from(
                "model request was already reserved; outcome may be unknown",
            ))?);
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
    let reservation_ms = started.elapsed().as_millis() as u64;
    // Exactly one application-level send; unknown provider outcomes retain the reservation.
    if wait > 0 {
        tokio::time::sleep(Duration::from_millis(wait as u64)).await;
    }
    validate_grant(&s, lease.clone()).await?;
    let suffix = expected.strip_prefix("v1/").unwrap();
    let base = cfg.base_url.trim_end_matches('/');
    let url = if base.ends_with("/v1") {
        format!("{base}/{suffix}")
    } else {
        format!("{base}/{expected}")
    };
    let timeout_ms = (life.deadline_ms.unwrap() - Utc::now().timestamp_millis()).max(1) as u64;
    let mut upstream = s
        .model_http
        .post(url)
        .body(body)
        .header("content-type", "application/json")
        .timeout(Duration::from_millis(timeout_ms));
    if cfg.api == "anthropic-messages" {
        upstream = upstream
            .header("x-api-key", &cfg.api_key)
            .header("anthropic-version", "2023-06-01");
    } else {
        upstream = upstream.bearer_auth(&cfg.api_key);
    }
    let send_started = std::time::Instant::now();
    let sending = upstream.send();
    tokio::pin!(sending);
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let response = loop {
        tokio::select! {
            response = &mut sending => break response.inspect_err(|error| {
                let mut cause = std::error::Error::source(error);
                let mut io_kind = None;
                let mut io_code = None;
                while let Some(source) = cause {
                    if let Some(io) = source.downcast_ref::<std::io::Error>() {
                        io_kind = Some(io.kind());
                        io_code = io.raw_os_error();
                        break;
                    }
                    cause = source.source();
                }
                tracing::warn!(
                    %run, generation, %request_id, reservation_ms, timeout_ms, body_bytes,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    upstream_elapsed_ms = send_started.elapsed().as_millis() as u64,
                    is_timeout = error.is_timeout(), is_connect = error.is_connect(),
                    is_request = error.is_request(), is_body = error.is_body(),
                    ?io_kind, io_code,
                    "model upstream failed before response headers"
                );
            })?,
            _ = tick.tick() => { validate_grant(&s, lease.clone()).await?; }
        }
    };
    let status = response.status();
    let content_type = response.headers().get("content-type").cloned();
    sqlx::query(
        "UPDATE runtime_model_requests SET state='response' WHERE run_id=$1 AND request_id=$2",
    )
    .bind(run)
    .bind(request_id)
    .execute(&s.pool)
    .await?;
    let mut result = Response::builder()
        .status(status)
        .header("cache-control", "no-store");
    if let Some(kind) = content_type {
        result = result.header("content-type", kind);
    }
    let stream = async_stream::try_stream! {
     use futures::StreamExt;
     let mut upstream=response.bytes_stream();let mut tick=tokio::time::interval(Duration::from_secs(2));
     loop {
      let event=tokio::select! {_=tick.tick()=>None, next=upstream.next()=>Some(next)};
      match event {None=>validate_grant(&s,lease.clone()).await.map_err(std::io::Error::other).map(|_|())?,Some(Some(chunk))=>yield chunk.map_err(std::io::Error::other)?,Some(None)=>break}
     }
    };
    let stream: std::pin::Pin<
        Box<dyn futures::Stream<Item = std::io::Result<axum::body::Bytes>> + Send>,
    > = Box::pin(stream);
    Ok(result.body(Body::from_stream(stream))?)
}
