//! Process-wide, bounded-cardinality data-plane metrics. No run/project IDs or secrets.
use axum::{body::Body, extract::Request, middleware::Next, response::Response};
use futures::StreamExt;
use std::{
    sync::atomic::{AtomicU64, Ordering::Relaxed},
    time::Instant,
};
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static ERRORS: AtomicU64 = AtomicU64::new(0);
static INFLIGHT: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static NANOS: AtomicU64 = AtomicU64::new(0);
struct Inflight(Instant);
impl Drop for Inflight {
    fn drop(&mut self) {
        INFLIGHT.fetch_sub(1, Relaxed);
        NANOS.fetch_add(
            self.0.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            Relaxed,
        );
    }
}
pub async fn observe(request: Request, next: Next) -> Response {
    if matches!(request.uri().path(), "/healthz" | "/metrics") {
        return next.run(request).await;
    }
    REQUESTS.fetch_add(1, Relaxed);
    INFLIGHT.fetch_add(1, Relaxed);
    let guard = Inflight(Instant::now());
    let response = next.run(request).await;
    if response.status().is_client_error() || response.status().is_server_error() {
        ERRORS.fetch_add(1, Relaxed);
    }
    let (parts, body) = response.into_parts();
    let stream = async_stream::stream! {
        let _guard = guard;
        let mut body = body.into_data_stream();
        while let Some(chunk) = body.next().await {
            if let Ok(ref bytes) = chunk { BYTES.fetch_add(bytes.len() as u64, Relaxed); }
            yield chunk;
        }
    };
    Response::from_parts(parts, Body::from_stream(stream))
}
pub fn render() -> String {
    format!(
        "# TYPE artifactory_service_requests_total counter\nartifactory_service_requests_total {}\n# TYPE artifactory_service_errors_total counter\nartifactory_service_errors_total {}\n# TYPE artifactory_service_inflight gauge\nartifactory_service_inflight {}\n# TYPE artifactory_service_response_bytes_total counter\nartifactory_service_response_bytes_total {}\n# TYPE artifactory_service_request_seconds_total counter\nartifactory_service_request_seconds_total {}\n",
        REQUESTS.load(Relaxed),
        ERRORS.load(Relaxed),
        INFLIGHT.load(Relaxed),
        BYTES.load(Relaxed),
        NANOS.load(Relaxed) as f64 / 1e9
    )
}
