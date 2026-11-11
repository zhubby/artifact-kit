mod agent_updates;
mod builds;
mod management;
mod projects;

use crate::{auth, state::AppState};
use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{delete, get, patch, post},
};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route(
            "/healthz",
            get(|| async {
                axum::Json(serde_json::json!({"status":"ok","version":env!("CARGO_PKG_VERSION")}))
            }),
        )
        .route("/api/v1/auth/login", post(auth::login))
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
        .route(
            "/api/v1/agent-releases",
            get(agent_updates::releases).post(agent_updates::import),
        )
        .route(
            "/api/v1/nodes/{id}/agent-update",
            patch(agent_updates::node_target),
        )
        .route(
            "/api/v1/pools/{id}/agent-update",
            patch(agent_updates::pool_target),
        )
        .route("/api/v1/agent-updates", get(agent_updates::updates))
        .route(
            "/api/v1/agent-updates/{id}/retry",
            post(agent_updates::retry),
        )
        .route(
            "/api/v1/agent-updates/{id}/cancel",
            post(agent_updates::cancel),
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
        .with_state(state)
}
