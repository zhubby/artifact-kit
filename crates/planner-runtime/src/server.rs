use crate::store::{ModelReservation, RuntimeMetrics, RuntimeStore, ToolDisposition};
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
use std::{sync::Arc, time::Duration};
use uuid::Uuid;
type ApiResult<T> = artifactory_runtime_protocol::error::Result<Json<T>>;
#[derive(Clone)]
pub struct Runtime {
    pub store: RuntimeStore,
    pub key: Arc<[u8; 32]>,
    pub control: Client,
    pub model_http: reqwest::Client,
    pub public_url: String,
}
pub async fn from_env() -> Result<Runtime> {
    let key = Arc::new(service::key("ARTIFACTORY_SERVICE_KEY")?);
    let store = RuntimeStore::connect(&service::required("DATABASE_URL")?, key.clone()).await?;
    Ok(Runtime {
        store,
        key,
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
    let stats = s.store.metrics().await?;
    for (status, runs) in stats.statuses {
        output.push_str(&format!(
            "artifactory_runtime_runs{{status=\"{status}\"}} {runs}\n"
        ));
    }
    let RuntimeMetrics {
        calls,
        pending,
        oldest,
        outbox,
        ..
    } = stats;
    output.push_str(&format!("artifactory_runtime_model_reservations_total {calls}\nartifactory_runtime_pending_proposals {pending}\nartifactory_runtime_oldest_queued_seconds {oldest}\n"));
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
    let ciphertext = if cancel {
        None
    } else {
        Some(crypto::seal(&state.key, &serde_json::to_vec(&value)?)?)
    };
    state
        .store
        .put_intent(run, version, hash, cancel, intent, ciphertext)
        .await
}
async fn intent(State(s): State<Runtime>, Json(v): Json<Value>) -> ApiResult<Value> {
    Ok(Json(put_intent(&s, v).await?))
}
pub async fn register_execution(s: &Runtime, execution: Execution) -> Result<Value> {
    s.store.register_execution(execution).await
}
async fn register(State(s): State<Runtime>, Json(v): Json<Execution>) -> ApiResult<Value> {
    Ok(Json(register_execution(&s, v).await?))
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
    s.store.acquire_execution(execution, &s.public_url).await
}
async fn acquire(State(s): State<Runtime>, Json(v): Json<Value>) -> ApiResult<Value> {
    Ok(Json(acquire_execution(&s, v).await?))
}
pub async fn heartbeat_run(s: &Runtime, lease: Lease) -> Result<Value> {
    s.store.heartbeat_run(lease).await
}
async fn heartbeat(State(s): State<Runtime>, Json(v): Json<Lease>) -> ApiResult<Value> {
    Ok(Json(heartbeat_run(&s, v).await?))
}
pub async fn validate_grant(s: &Runtime, lease: Lease) -> Result<Value> {
    s.store.validate_grant(lease).await
}
async fn validate(State(s): State<Runtime>, Json(v): Json<Lease>) -> ApiResult<Value> {
    Ok(Json(validate_grant(&s, v).await?))
}
pub async fn read_run(s: &Runtime, run: Uuid) -> Result<Value> {
    s.store.read_run(run).await
}
async fn get_run(State(s): State<Runtime>, Json(v): Json<Value>) -> ApiResult<Value> {
    Ok(Json(read_run(&s, id(&v, "run_id")?).await?))
}
pub async fn report_event(s: &Runtime, v: Value) -> Result<Value> {
    s.store.report_event(v).await
}
async fn report(State(s): State<Runtime>, Json(v): Json<Value>) -> ApiResult<Value> {
    Ok(Json(report_event(&s, v).await?))
}

pub async fn tool_call(s: &Runtime, v: Value) -> Result<Value> {
    let lease: Lease = serde_json::from_value(v["lease"].clone())?;
    let name = v["name"].as_str().context("tool name required")?;
    let proposal = id(&v, "request_id")?;
    match s.store.tool_call(lease, name, proposal, &v).await? {
        ToolDisposition::Immediate(value) => Ok(value),
        ToolDisposition::Forward(input) => s.control.post("/internal/runtime/tool", &input).await,
    }
}
async fn tool(State(s): State<Runtime>, Json(v): Json<Value>) -> ApiResult<Value> {
    Ok(Json(tool_call(&s, v).await?))
}
async fn proposal_result(State(s): State<Runtime>, Json(v): Json<Value>) -> ApiResult<Value> {
    let lease: Lease = serde_json::from_value(v["lease"].clone())?;
    // An expired owner may only query its already registered proposal; it cannot create a new one.
    Ok(Json(
        s.store
            .proposal_result(id(&v, "proposal_id")?, lease)
            .await?,
    ))
}
async fn queue(State(s): State<Runtime>, Json(v): Json<Value>) -> ApiResult<Value> {
    let pool = v["pool"].as_str().unwrap_or("default");
    let limit = v["limit"].as_i64().unwrap_or(8).clamp(0, 1000);
    let per_project = v["per_project"]
        .as_i64()
        .unwrap_or(4)
        .clamp(1, limit.max(1));
    Ok(Json(s.store.queue(pool, limit, per_project).await?))
}
async fn reconcile(State(s): State<Runtime>, Json(_): Json<Value>) -> ApiResult<Value> {
    maintain(&s).await?;
    Ok(Json(json!({"ok":true})))
}
pub async fn maintain(s: &Runtime) -> Result<()> {
    s.store.maintain().await?;
    deliver(s).await
}
pub async fn deliver(s: &Runtime) -> Result<()> {
    let rows = s.store.deliveries().await?;
    for row in rows {
        let event = row.id;
        let run = row.run_id;
        let kind = row.kind;
        let payload = row.payload;
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
        s.store
            .acknowledge_delivery(event, run, &kind, &payload, &result)
            .await?;
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
            } else {
                crate::store::database_failure_category(&error).unwrap_or("run_validation")
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
    let ModelReservation::Reserved {
        config: cfg,
        deadline_ms,
        wait_ms: wait,
        endpoint: expected,
    } = s
        .store
        .reserve_model(crate::store::ModelRequest {
            run,
            generation,
            lease: &lease,
            request_id,
            path: &path,
            value: &value,
            hash: crypto::hash(&body),
        })
        .await?
    else {
        return Ok(Response::builder()
            .status(StatusCode::CONFLICT)
            .body(Body::from(
                "model request was already reserved; outcome may be unknown",
            ))?);
    };
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
    let timeout_ms = (deadline_ms - Utc::now().timestamp_millis()).max(1) as u64;
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
    s.store.model_response_received(run, request_id).await?;
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
