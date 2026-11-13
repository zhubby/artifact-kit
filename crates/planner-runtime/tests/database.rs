use artifactory_database::{PoolSettings, connect, with_options};
use artifactory_database::{params, sql};
use artifactory_planner_runtime::{
    lifecycle::Lifecycle,
    server::{self, Runtime},
};
use artifactory_runtime_protocol::{
    Execution, Lease, ModelConfiguration, RunIntent, crypto, service::Client,
};
use serde_json::{Value, json};
use std::sync::Arc;
use toasty::stmt::Type;
use uuid::Uuid;
struct TestRuntime {
    runtime: Runtime,
    pool: toasty::Db,
}
impl std::ops::Deref for TestRuntime {
    type Target = Runtime;
    fn deref(&self) -> &Runtime {
        &self.runtime
    }
}
impl TestRuntime {
    fn clone(&self) -> Runtime {
        self.runtime.clone()
    }
}
async fn state() -> TestRuntime {
    let url = std::env::var("ARTIFACTORY_TEST_DATABASE_URL")
        .expect("ARTIFACTORY_TEST_DATABASE_URL required");
    let admin = connect(&url, Default::default(), PoolSettings::new(2))
        .await
        .unwrap();
    let schema = format!("rt_{}", Uuid::new_v4().simple());
    sql::execute(
        &mut admin.clone(),
        &format!("CREATE SCHEMA {schema}"),
        params![],
    )
    .await
    .unwrap();
    let scoped = with_options(&url, &[("search_path", &schema)]).unwrap();
    let pool = connect(
        &scoped,
        artifactory_planner_runtime::store::models(),
        PoolSettings::new(12),
    )
    .await
    .unwrap();
    artifactory_database::migrate(&pool, &artifactory_planner_runtime::store::MIGRATIONS)
        .await
        .unwrap();
    let runtime = Runtime {
        store: artifactory_planner_runtime::store::RuntimeStore::new(
            pool.clone(),
            Arc::new([9; 32]),
        ),
        key: Arc::new([9; 32]),
        control: Client::new("https://control.invalid".into(), "x".repeat(64)).unwrap(),
        model_http: artifactory_runtime_protocol::service::client().unwrap(),
        public_url: "https://runtime.invalid".into(),
    };
    TestRuntime { runtime, pool }
}
fn intent(run: Uuid) -> RunIntent {
    RunIntent {
        run_id: run,
        build_id: Uuid::new_v4(),
        project_id: Uuid::new_v4(),
        version: 1,
        snapshot_id: Uuid::new_v4(),
        snapshot_digest: "a".repeat(64),
        pool: "default".into(),
        context: json!({}),
        model: ModelConfiguration {
            id: Uuid::new_v4(),
            api: "openai-completions".into(),
            base_url: "https://model.invalid/v1".into(),
            model: "fixture".into(),
            api_key: "private-provider-key".into(),
            parameters: json!({}),
            max_calls: 2,
            timeout_seconds: 600,
        },
    }
}
async fn owner(s: &TestRuntime, run: Uuid) -> Lease {
    let execution = Execution {
        run_id: run,
        generation: 1,
        uid: "pod-a".into(),
        name: "pod-a".into(),
        namespace: "test".into(),
    };
    server::register_execution(s, execution).await.unwrap();
    let mut life = Lifecycle::new(2);
    life.acquire(1, "pod-a", chrono::Utc::now().timestamp_millis(), 600000)
        .unwrap();
    let token = crypto::token();
    sql::execute(
        &mut s.pool.clone(),
        "UPDATE runtime_runs SET status='running',lifecycle=$2,grant_hash=$3 WHERE id=$1",
        params![
            run,
            serde_json::to_value(life).unwrap(),
            crypto::hash(&token)
        ],
    )
    .await
    .unwrap();
    Lease {
        run_id: run,
        generation: 1,
        uid: "pod-a".into(),
        token,
    }
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn intents_are_idempotent_conflicting_payloads_fail_and_cancel_before_create_cannot_resurrect()
 {
    let s = state().await;
    let run = Uuid::new_v4();
    let value = serde_json::to_value(intent(run)).unwrap();
    server::put_intent(&s, value.clone()).await.unwrap();
    server::put_intent(&s, value.clone()).await.unwrap();
    let mut other = value.clone();
    other["pool"] = json!("other");
    assert!(server::put_intent(&s, other).await.is_err());
    let ciphertext: String = sql::scalar(
        &mut s.pool.clone(),
        "SELECT intent_ciphertext FROM runtime_runs WHERE id=$1",
        params![run],
    )
    .await
    .unwrap();
    assert!(!ciphertext.contains("private-provider-key"));
    let canceled = Uuid::new_v4();
    server::put_intent(&s, json!({"run_id":canceled,"version":2,"canceled":true}))
        .await
        .unwrap();
    server::put_intent(&s, serde_json::to_value(intent(canceled)).unwrap())
        .await
        .unwrap();
    assert_eq!(
        server::read_run(&s, canceled).await.unwrap()["status"],
        "canceled"
    );
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn duplicate_pod_and_old_generation_cannot_report_or_submit_and_proposal_survives_ack_loss() {
    let s = state().await;
    let run = Uuid::new_v4();
    server::put_intent(&s, serde_json::to_value(intent(run)).unwrap())
        .await
        .unwrap();
    let lease = owner(&s, run).await;
    assert!(
        server::register_execution(
            &s,
            Execution {
                run_id: run,
                generation: 1,
                uid: "pod-b".into(),
                name: "pod-b".into(),
                namespace: "test".into()
            }
        )
        .await
        .is_err()
    );
    let id = Uuid::new_v4();
    let value = json!({"lease":lease,"request_id":id,"name":"submit_plan","arguments":{"plan":{"summary":"test"}}});
    assert_eq!(
        server::tool_call(&s, value.clone()).await.unwrap()["pending"],
        true
    );
    assert_eq!(
        server::tool_call(&s, value.clone()).await.unwrap()["proposal_id"],
        id.to_string()
    );
    let mut conflict = value;
    conflict["arguments"] = json!({});
    assert!(server::tool_call(&s, conflict).await.is_err());
    let proposals: i64 = sql::scalar(
        &mut s.pool.clone(),
        "SELECT count(*) FROM runtime_proposals WHERE run_id=$1",
        params![run],
    )
    .await
    .unwrap();
    assert_eq!(proposals, 1);
    let mut stale = lease.clone();
    stale.uid = "pod-b".into();
    assert!(server::validate_grant(&s, stale).await.is_err());
    server::put_intent(&s, json!({"run_id":run,"version":2,"canceled":true}))
        .await
        .unwrap();
    server::put_intent(&s, json!({"run_id":run,"version":2,"canceled":true}))
        .await
        .expect("replaying cancellation must be idempotent");
    assert!(server::heartbeat_run(&s, lease).await.is_err());
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn replayed_and_out_of_order_checkpoints_do_not_overwrite_newer_state() {
    let s = state().await;
    let run = Uuid::new_v4();
    server::put_intent(&s, serde_json::to_value(intent(run)).unwrap())
        .await
        .unwrap();
    let lease = owner(&s, run).await;
    let event = json!({"lease":lease,"event_id":Uuid::new_v4(),"sequence":2,"kind":"checkpoint","payload":{"messages":"newer"}});
    server::report_event(&s, event.clone()).await.unwrap();
    server::report_event(&s, event).await.unwrap();
    server::report_event(&s,json!({"lease":lease,"event_id":Uuid::new_v4(),"sequence":1,"kind":"checkpoint","payload":{"messages":"old"}})).await.unwrap();
    let row = sql::record(
        &mut s.pool.clone(),
        "SELECT checkpoint FROM runtime_runs WHERE id=$1",
        params![run],
        &[("checkpoint", Type::String)],
    )
    .await
    .unwrap();
    assert_eq!(
        row.decode::<Value>("checkpoint")
            .expect("database projection")["messages"],
        "newer"
    );
}

type Requests = Arc<std::sync::Mutex<Vec<(String, String, Value)>>>;
async fn upstream() -> (String, Requests, tokio::task::JoinHandle<()>) {
    use axum::{
        Router,
        extract::{Path, State},
        http::HeaderMap,
        routing::post,
    };
    let requests: Requests = Default::default();
    let captured = requests.clone();
    let app = Router::new()
        .route(
            "/{*path}",
            post(
                |State(seen): State<Requests>,
                 Path(path): Path<String>,
                 headers: HeaderMap,
                 axum::Json(value): axum::Json<Value>| async move {
                    let key = headers
                        .get("authorization")
                        .or_else(|| headers.get("x-api-key"))
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_string();
                    seen.lock().unwrap().push((path, key, value));
                    (
                        [("content-type", "text/event-stream")],
                        "event: tool\ndata: {\"arguments\":\"partial\"}\n\ndata: [DONE]\n\n",
                    )
                },
            ),
        )
        .with_state(captured);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (origin, requests, handle)
}
async fn fixture_run(s: &TestRuntime, origin: &str, api: &str) -> (Uuid, Lease) {
    let run = Uuid::new_v4();
    let mut input = intent(run);
    input.model.api = api.into();
    server::put_intent(s, serde_json::to_value(&input).unwrap())
        .await
        .unwrap();
    // Only the test upstream is plaintext; production endpoint validation remains enabled.
    input.model.base_url = format!("{origin}/v1");
    sql::execute(
        &mut s.pool.clone(),
        "UPDATE runtime_runs SET intent_ciphertext=$2 WHERE id=$1",
        params![
            run,
            crypto::seal(&s.key, &serde_json::to_vec(&input).unwrap()).unwrap()
        ],
    )
    .await
    .unwrap();
    (run, owner(s, run).await)
}
fn model_request(lease: &Lease, request_id: Uuid) -> axum::extract::Request {
    axum::http::Request::builder()
        .header("authorization", format!("Bearer {}", lease.token))
        .header("x-artifactory-request-id", request_id.to_string())
        .body(axum::body::Body::from(
            json!({"model":"fixture","stream":true,"messages":[],"tools":[]}).to_string(),
        ))
        .unwrap()
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn all_model_protocols_stream_with_private_credentials_and_replays_never_resend() {
    let s = state().await;
    let (origin, requests, task) = upstream().await;
    for (api, path) in [
        ("openai-completions", "v1/chat/completions"),
        ("openai-responses", "v1/responses"),
        ("anthropic-messages", "v1/messages"),
    ] {
        let (run, lease) = fixture_run(&s, &origin, api).await;
        let request_id = Uuid::new_v4();
        let reply = server::model_request(
            s.clone(),
            run,
            1,
            lease.uid.clone(),
            path.into(),
            model_request(&lease, request_id),
        )
        .await
        .unwrap();
        assert_eq!(reply.status(), 200);
        assert_eq!(reply.headers()["content-type"], "text/event-stream");
        let body = axum::body::to_bytes(reply.into_body(), 4096).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("partial"));
        let replay = server::model_request(
            s.clone(),
            run,
            1,
            lease.uid.clone(),
            path.into(),
            model_request(&lease, request_id),
        )
        .await
        .unwrap();
        assert_eq!(replay.status(), 409);
        assert!(
            server::model_request(
                s.clone(),
                run,
                1,
                lease.uid.clone(),
                "v1/other".into(),
                model_request(&lease, Uuid::new_v4())
            )
            .await
            .is_err()
        );
        assert_eq!(
            server::read_run(&s, run).await.unwrap()["lifecycle"]["calls"],
            1
        );
    }
    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 3);
    assert_eq!(captured[0].0, "v1/chat/completions");
    assert_eq!(captured[1].0, "v1/responses");
    assert_eq!(captured[2].0, "v1/messages");
    assert_eq!(captured[0].1, "Bearer private-provider-key");
    assert_eq!(captured[2].1, "private-provider-key");
    task.abort();
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn concurrent_model_requests_cannot_overspend_and_unknown_outcomes_remain_consumed() {
    let s = state().await;
    let (origin, requests, task) = upstream().await;
    let (run, lease) = fixture_run(&s, &origin, "openai-completions").await;
    let results = futures::future::join_all((0..12).map(|_| {
        server::model_request(
            s.clone(),
            run,
            1,
            lease.uid.clone(),
            "v1/chat/completions".into(),
            model_request(&lease, Uuid::new_v4()),
        )
    }))
    .await;
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 2);
    assert_eq!(requests.lock().unwrap().len(), 2);
    task.abort();
    let (run, lease) = fixture_run(&s, &origin, "openai-completions").await;
    let request_id = Uuid::new_v4();
    assert!(
        server::model_request(
            s.clone(),
            run,
            1,
            lease.uid.clone(),
            "v1/chat/completions".into(),
            model_request(&lease, request_id)
        )
        .await
        .is_err()
    );
    let replay = server::model_request(
        s.clone(),
        run,
        1,
        lease.uid.clone(),
        "v1/chat/completions".into(),
        model_request(&lease, request_id),
    )
    .await
    .unwrap();
    assert_eq!(replay.status(), 409);
    assert_eq!(
        server::read_run(&s, run).await.unwrap()["lifecycle"]["calls"],
        1
    );
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn ten_thousand_queued_runs_admit_only_pool_capacity_even_with_concurrent_dispatchers() {
    let s = state().await;
    let projects: Vec<_> = (0..4).map(|_| Uuid::new_v4()).collect();
    let mut ids = Vec::new();
    let mut encrypted = Vec::new();
    let mut project_ids = Vec::new();
    let mut model = None;
    for i in 0..10_000 {
        let run = Uuid::new_v4();
        let mut input = intent(run);
        input.project_id = projects[i % 4];
        model = Some(input.snapshot_id);
        ids.push(run);
        project_ids.push(input.project_id);
        encrypted.push(crypto::seal(&s.key, &serde_json::to_vec(&input).unwrap()).unwrap());
    }
    sql::execute(&mut s.pool.clone(), "INSERT INTO runtime_runs(id,version,intent_hash,intent_ciphertext,project_id,pool,snapshot_id,snapshot_digest,lifecycle,status) SELECT id,1,'fixture',ciphertext,project,'default',$4,$5,$6,'queued' FROM unnest($1::uuid[],$2::text[],$3::uuid[]) AS t(id,ciphertext,project)", params![&ids, &encrypted, &project_ids, model.unwrap(), "a".repeat(64), serde_json::to_value(Lifecycle::new(2)).unwrap()]).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/queue", listener.local_addr().unwrap());
    let app = server::router(s.clone(), "x".repeat(64));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let http = reqwest::Client::new();
    for _ in 0..4 {
        let replies = futures::future::join_all((0..3).map(|_| {
            http.post(&url)
                .bearer_auth("x".repeat(64))
                .json(&json!({"pool":"default","limit":8,"per_project":2}))
                .send()
        }))
        .await;
        for reply in replies {
            let value: Value = reply
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert!(value["runs"].as_array().unwrap().len() <= 8);
        }
    }
    let active: i64 = sql::scalar(
        &mut s.pool.clone(),
        "SELECT count(*) FROM runtime_runs WHERE status='provisioning'",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(active, 8);
    let highest:i64=sql::scalar(&mut s.pool.clone(), "SELECT max(n)::bigint FROM (SELECT count(*) n FROM runtime_runs WHERE status='provisioning' GROUP BY project_id) c", params![]).await.unwrap();
    assert_eq!(highest, 2);
    let untouched:i64=sql::scalar(&mut s.pool.clone(), "SELECT count(*) FROM runtime_runs WHERE status='queued' AND (lifecycle->>'deadline_ms') IS NULL AND (lifecycle->>'calls')::int=0", params![]).await.unwrap();
    assert_eq!(untouched, 9_992);
    task.abort();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn cancellation_interrupts_a_provider_that_has_not_returned_response_headers() {
    let s = state().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            "late response"
        }),
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (run, lease) = fixture_run(&s, &origin, "openai-completions").await;
    let (result, ()) = tokio::join!(
        tokio::time::timeout(
            std::time::Duration::from_secs(4),
            server::model_request(
                s.clone(),
                run,
                1,
                lease.uid.clone(),
                "v1/chat/completions".into(),
                model_request(&lease, Uuid::new_v4())
            )
        ),
        async {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            server::put_intent(&s, json!({"run_id":run,"version":2,"canceled":true}))
                .await
                .unwrap();
        }
    );
    assert!(
        result
            .expect("cancel must interrupt the upstream wait")
            .is_err()
    );
    assert_eq!(
        server::read_run(&s, run).await.unwrap()["lifecycle"]["calls"],
        1
    );
    task.abort();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn profile_rate_reservations_are_shared_across_different_runs() {
    let s = state().await;
    let (origin, _, task) = upstream().await;
    let profile = Uuid::new_v4();
    let mut runs = Vec::new();
    for _ in 0..3 {
        let (run, lease) = fixture_run(&s, &origin, "openai-completions").await;
        let ciphertext: String = sql::scalar(
            &mut s.pool.clone(),
            "SELECT intent_ciphertext FROM runtime_runs WHERE id=$1",
            params![run],
        )
        .await
        .unwrap();
        let mut cfg: RunIntent =
            serde_json::from_slice(&crypto::open(&s.key, &ciphertext).unwrap()).unwrap();
        cfg.model.id = profile;
        cfg.model.parameters = json!({"requests_per_minute":600});
        sql::execute(
            &mut s.pool.clone(),
            "UPDATE runtime_runs SET intent_ciphertext=$2 WHERE id=$1",
            params![
                run,
                crypto::seal(&s.key, &serde_json::to_vec(&cfg).unwrap()).unwrap()
            ],
        )
        .await
        .unwrap();
        runs.push((run, lease));
    }
    let began = std::time::Instant::now();
    let results = futures::future::join_all(runs.iter().map(|(run, lease)| {
        server::model_request(
            s.clone(),
            *run,
            1,
            lease.uid.clone(),
            "v1/chat/completions".into(),
            model_request(lease, Uuid::new_v4()),
        )
    }))
    .await;
    assert!(results.iter().all(|result| result.is_ok()));
    assert!(
        began.elapsed() >= std::time::Duration::from_secs(2),
        "a shared profile must serialize reserved rate slots"
    );
    task.abort();
}
