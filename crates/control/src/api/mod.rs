pub(crate) mod ai_management;
mod builds;
mod emissary_updates;
mod management;
mod projects;
mod source_http;
mod telemetry;

use crate::{auth, state::AppState};
use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{delete, get, patch, post},
};

pub fn router(state: AppState) -> Router {
    health_routes()
        .route("/api/v1/model-options", get(ai_management::model_options))
        .route(
            "/api/v1/model-profiles",
            get(ai_management::profiles).post(ai_management::create_profile),
        )
        .route(
            "/api/v1/model-profiles/{id}",
            patch(ai_management::update_profile).delete(ai_management::delete_profile),
        )
        .route("/api/v1/ai-workers", get(ai_management::workers))
        .route(
            "/api/v1/ai-workers/{id}",
            patch(ai_management::update_worker),
        )
        .route(
            "/api/v1/ai-enrollment-tokens",
            post(ai_management::enrollment),
        )
        .route("/api/v1/source-metrics", get(ai_management::source_metrics))
        .route(
            "/api/v1/builds/{id}/clarifications",
            post(ai_management::clarify),
        )
        .route("/api/v1/builds/{id}/plans", get(ai_management::plans))
        .merge(source_routes())
        .route("/api/v1/auth/login", post(auth::login))
        .route("/api/v1/auth/config", get(auth::registration_config))
        .route("/api/v1/auth/register", post(auth::register))
        .route("/api/v1/auth/logout", post(auth::logout))
        .route("/api/v1/auth/me", get(auth::me))
        .route("/api/v1/overview", get(management::overview))
        .route(
            "/api/v1/users",
            get(management::users).post(management::create_user),
        )
        .route(
            "/api/v1/tokens",
            get(management::tokens).post(management::create_token),
        )
        .route("/api/v1/tokens/{id}", delete(management::revoke_token))
        .route(
            "/api/v1/pools",
            get(management::pools).post(management::create_pool),
        )
        .route(
            "/api/v1/enrollment-tokens",
            post(management::enrollment_token),
        )
        .route("/api/v1/nodes", get(management::nodes))
        .route("/api/v1/nodes/{id}", patch(management::update_node))
        .route("/api/v1/nodes/{id}/telemetry", get(telemetry::latest))
        .route(
            "/api/v1/nodes/{id}/telemetry/history",
            get(telemetry::history),
        )
        .route("/api/v1/nodes/{id}/inventory", get(telemetry::inventory))
        .route(
            "/api/v1/attempts/{id}/telemetry",
            get(telemetry::build_history),
        )
        .route("/api/v1/attempts/{id}/processes", get(telemetry::processes))
        .route(
            "/api/v1/emissary-releases",
            get(emissary_updates::releases).post(emissary_updates::import),
        )
        .route(
            "/api/v1/nodes/{id}/emissary-update",
            patch(emissary_updates::node_target),
        )
        .route(
            "/api/v1/pools/{id}/emissary-update",
            patch(emissary_updates::pool_target),
        )
        .route("/api/v1/emissary-updates", get(emissary_updates::updates))
        .route(
            "/api/v1/emissary-updates/{id}/retry",
            post(emissary_updates::retry),
        )
        .route(
            "/api/v1/emissary-updates/{id}/cancel",
            post(emissary_updates::cancel),
        )
        .route(
            "/api/v1/credentials",
            get(management::credentials).post(management::create_credential),
        )
        .route("/api/v1/audit", get(management::audit))
        .route(
            "/api/v1/projects",
            get(projects::list).post(projects::create),
        )
        .route(
            "/api/v1/projects/{id}",
            get(projects::detail).patch(projects::update),
        )
        .route("/api/v1/projects/{id}/builds", post(builds::create))
        .route("/api/v1/projects/{id}/refs", get(projects::refs))
        .route("/api/v1/builds", get(builds::list))
        .route("/api/v1/builds/{id}", get(builds::detail))
        .route("/api/v1/builds/{id}/cancel", post(builds::cancel))
        .route("/api/v1/builds/{id}/publish", post(builds::publish))
        .route("/api/v1/builds/{id}/events", get(builds::events))
        .route("/api/v1/jobs/{id}/retry", post(builds::retry))
        .route("/api/v1/artifacts", get(builds::artifacts))
        .route("/api/v1/artifacts/{id}/download", get(builds::download))
        .route("/api/v1/releases", get(builds::releases))
        .route("/api/v1/releases/{id}", get(builds::release_detail))
        .layer(DefaultBodyLimit::max(512 * 1024))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state.clone())
        .merge(crate::runtime_bridge::routes(state))
}

pub(crate) fn health_routes() -> Router<AppState> {
    Router::new().route("/readyz", get(readiness)).route(
        "/healthz",
        get(|| async {
            axum::Json(serde_json::json!({"status":"ok","version":env!("CARGO_PKG_VERSION")}))
        }),
    )
}

pub(crate) fn source_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/source/{snapshot}/{entry}/git/info/refs",
            get(source_http::git),
        )
        .route(
            "/api/v1/source/{snapshot}/{entry}/git/git-upload-pack",
            post(source_http::git),
        )
        .route(
            "/api/v1/source/{snapshot}/{entry}/lfs/objects/batch",
            post(source_http::lfs_batch),
        )
        .route(
            "/api/v1/source/{snapshot}/{entry}/lfs/objects/{oid}",
            get(source_http::lfs_object),
        )
        .layer(DefaultBodyLimit::disable())
        .layer(axum::middleware::map_response(source_http::challenge))
}

async fn readiness(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::http::StatusCode {
    if state.coordination.is_draining() {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE;
    }
    match tokio::time::timeout(
        std::time::Duration::from_secs(1),
        sqlx::query("SELECT 1").execute(&state.pool),
    )
    .await
    {
        Ok(Ok(_)) => axum::http::StatusCode::OK,
        _ => axum::http::StatusCode::SERVICE_UNAVAILABLE,
    }
}
