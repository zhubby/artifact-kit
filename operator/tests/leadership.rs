use artifactory_operator::leadership::Leadership;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use k8s_openapi::api::coordination::v1::Lease;
use kube::{Api, Client, Config};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct ApiState {
    lease: Arc<Mutex<Option<Lease>>>,
    writes: Arc<std::sync::atomic::AtomicUsize>,
    unavailable: Arc<std::sync::atomic::AtomicBool>,
}
fn status(code: StatusCode) -> Response {
    let reason = match code {
        StatusCode::NOT_FOUND => "NotFound",
        StatusCode::CONFLICT => "Conflict",
        StatusCode::FORBIDDEN => "Forbidden",
        _ => "ServiceUnavailable",
    };
    (code, Json(json!({"apiVersion":"v1","kind":"Status","status":"Failure","code":code.as_u16(),"reason":reason,"message":"injected Kubernetes API failure"}))).into_response()
}
async fn read(State(state): State<ApiState>) -> Response {
    if state.unavailable.load(std::sync::atomic::Ordering::SeqCst) {
        return status(StatusCode::FORBIDDEN);
    }
    match state.lease.lock().unwrap().clone() {
        Some(lease) => Json(lease).into_response(),
        None => status(StatusCode::NOT_FOUND),
    }
}
async fn write(State(state): State<ApiState>, Json(mut desired): Json<Lease>) -> Response {
    let mut current = state.lease.lock().unwrap();
    if current
        .as_ref()
        .and_then(|l| l.metadata.resource_version.as_ref())
        != desired.metadata.resource_version.as_ref()
    {
        return status(StatusCode::CONFLICT);
    }
    let revision = state
        .writes
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        + 1;
    desired.metadata.resource_version = Some(revision.to_string());
    *current = Some(desired.clone());
    Json(desired).into_response()
}
async fn api() -> (ApiState, Api<Lease>, tokio::task::JoinHandle<()>) {
    let state = ApiState::default();
    let app = Router::new()
        .route(
            "/apis/coordination.k8s.io/v1/namespaces/test/leases",
            post(write),
        )
        .route(
            "/apis/coordination.k8s.io/v1/namespaces/test/leases/operator",
            get(read).put(write),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = Config::new(
        format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    );
    let client = Client::try_from(config).unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (state, Api::namespaced(client, "test"), server)
}
fn candidate(api: Api<Lease>, identity: &str) -> Leadership {
    Leadership {
        api,
        identity: Arc::new(identity.to_owned()),
        name: "operator".into(),
    }
}

#[tokio::test]
async fn concurrent_candidates_have_one_owner_and_follower_cannot_mutate() {
    let (state, api, server) = api().await;
    let a = candidate(api.clone(), "pod-a");
    let b = candidate(api, "pod-b");
    let (first, second) = tokio::join!(a.renew(), b.renew());
    assert_ne!(first.unwrap(), second.unwrap());
    assert_eq!(state.writes.load(std::sync::atomic::Ordering::SeqCst), 1);
    let owner = state
        .lease
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .spec
        .as_ref()
        .unwrap()
        .holder_identity
        .clone()
        .unwrap();
    let (leader, follower) = if owner == "pod-a" { (&a, &b) } else { (&b, &a) };
    leader.ensure().await.unwrap();
    assert!(follower.ensure().await.is_err());
    assert!(!follower.renew().await.unwrap());
    assert_eq!(state.writes.load(std::sync::atomic::Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn expired_or_unreadable_lease_blocks_mutation_and_replacement_fences_old_owner() {
    use k8s_openapi::{
        apimachinery::pkg::apis::meta::v1::MicroTime,
        jiff::{SignedDuration, Timestamp},
    };
    let (state, api, server) = api().await;
    let old = candidate(api.clone(), "old-pod");
    let replacement = candidate(api, "replacement-pod");
    assert!(old.renew().await.unwrap());
    state
        .unavailable
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(
        old.ensure().await.is_err(),
        "API loss cannot use cached leadership"
    );
    state
        .unavailable
        .store(false, std::sync::atomic::Ordering::SeqCst);
    state
        .lease
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .spec
        .as_mut()
        .unwrap()
        .renew_time = Some(MicroTime(Timestamp::now() - SignedDuration::from_secs(30)));
    assert!(
        old.ensure().await.is_err(),
        "stale ownership cannot authorize writes"
    );
    assert!(replacement.renew().await.unwrap());
    replacement.ensure().await.unwrap();
    assert!(old.ensure().await.is_err());
    assert!(!old.renew().await.unwrap());
    server.abort();
}
