//! Queries are bounded by time and row count and use an independent read pool.
use crate::Service;
use artifactory_runtime_protocol::{
    error::{ApiError, Result},
    service,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::{get, post},
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Available historical resolutions and their exact retention boundaries.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Raw,
    #[default]
    Minute,
    Hour,
}

/// Required explicit range prevents accidental unbounded hypertable scans.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    #[serde(default)]
    pub resolution: Resolution,
}
impl Range {
    fn check(&self, process: bool) -> Result<()> {
        let (days, max_span) = match self.resolution {
            Resolution::Raw => (if process { 3 } else { 7 }, Duration::hours(6)),
            Resolution::Minute => (30, Duration::days(2)),
            Resolution::Hour => (180, Duration::days(31)),
        };
        if self.from >= self.to
            || self.to - self.from > max_span
            || self.from < Utc::now() - Duration::days(days)
            || self.to > Utc::now() + Duration::minutes(5)
        {
            return Err(ApiError::BadRequest(
                "invalid or expired telemetry range; raw <=6h, minute <=2d, hour <=31d".into(),
            ));
        }
        Ok(())
    }
}

/// Internal service routes, always protected by a dedicated service token.
pub fn router(state: Service, token: String) -> Router {
    service::protected(
        Router::new()
            .route("/metrics", get(|| async { crate::metrics() }))
            .route("/v1/resources", post(resources))
            .route("/v1/nodes/{id}/latest", get(latest))
            .route("/v1/nodes/{id}/history", get(history))
            .route("/v1/nodes/{id}/inventory", get(inventory))
            .route("/v1/attempts/{id}/history", get(build_history))
            .route("/v1/attempts/{id}/processes", get(processes))
            .with_state(state),
        token,
    )
}

async fn resources(
    State(s): State<Service>,
    Json(nodes): Json<Vec<artifactory_telemetry_model::nodes::ResourceIdentity>>,
) -> Result<Json<Value>> {
    if nodes.len() > 512 {
        return Err(ApiError::BadRequest(
            "at most 512 resource identities".into(),
        ));
    }
    Ok(Json(serde_json::to_value(
        s.store.resources(&nodes).await?,
    )?))
}
async fn latest(State(s): State<Service>, Path(id): Path<Uuid>) -> Result<Json<Value>> {
    Ok(Json(serde_json::to_value(s.store.latest(id).await?)?))
}
async fn inventory(State(s): State<Service>, Path(id): Path<Uuid>) -> Result<Json<Value>> {
    Ok(Json(serde_json::to_value(s.store.inventory(id).await?)?))
}
async fn history(
    State(s): State<Service>,
    Path(id): Path<Uuid>,
    Query(r): Query<Range>,
) -> Result<Json<Value>> {
    r.check(false)?;
    Ok(Json(serde_json::to_value(s.store.history(id, &r).await?)?))
}
async fn build_history(
    State(s): State<Service>,
    Path(id): Path<Uuid>,
    Query(r): Query<Range>,
) -> Result<Json<Value>> {
    r.check(true)?;
    Ok(Json(serde_json::to_value(
        s.store.build_history(id, &r).await?,
    )?))
}
async fn processes(
    State(s): State<Service>,
    Path(id): Path<Uuid>,
    Query(r): Query<Range>,
) -> Result<Json<Value>> {
    r.check(true)?;
    Ok(Json(serde_json::to_value(
        s.store.processes(id, &r).await?,
    )?))
}
