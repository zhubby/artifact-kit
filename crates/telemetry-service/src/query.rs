//! Queries are bounded by time and row count and use an independent read pool.
use crate::Service;
use artifactory_runtime_protocol::{
    error::{ApiError, Result},
    service,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use chrono::{DateTime, Duration, Utc};
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

async fn bounded(
    query: sqlx::query::QueryScalar<'_, sqlx::Postgres, Value, sqlx::postgres::PgArguments>,
    pool: &sqlx::PgPool,
    limit: usize,
) -> Result<Json<Value>> {
    let mut stream = query.fetch(pool);
    let mut samples = Vec::new();
    let mut bytes = 0;
    let mut truncated = false;
    while let Some(value) = stream.try_next().await? {
        bytes += serde_json::to_vec(&value)?.len();
        if bytes > 2 * 1024 * 1024 || samples.len() >= limit {
            truncated = true;
            break;
        }
        samples.push(value);
    }
    Ok(Json(json!({"samples":samples,"truncated":truncated})))
}

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
            .route("/v1/nodes/{id}/latest", get(latest))
            .route("/v1/nodes/{id}/history", get(history))
            .route("/v1/nodes/{id}/inventory", get(inventory))
            .route("/v1/attempts/{id}/history", get(build_history))
            .route("/v1/attempts/{id}/processes", get(processes))
            .with_state(state),
        token,
    )
}
async fn latest(State(s): State<Service>, Path(id): Path<Uuid>) -> Result<Json<Value>> {
    let host:Option<Value>=sqlx::query_scalar("SELECT to_jsonb(h) || jsonb_build_object('fresh', sampled_at BETWEEN now()-interval '90 seconds' AND now()+interval '30 seconds' AND received_at>now()-interval '90 seconds') FROM latest_hosts h WHERE node_id=$1")
        .bind(id).fetch_optional(&s.read).await?;
    let inventory:Option<Value>=sqlx::query_scalar("SELECT jsonb_build_object('sampled_at',i.sampled_at,'digest',i.digest,'inventory',v.inventory) FROM latest_inventories i JOIN inventory_versions v USING(node_id,digest) WHERE i.node_id=$1")
        .bind(id).fetch_optional(&s.read).await?;
    Ok(Json(json!({"host":host,"inventory":inventory})))
}
async fn inventory(State(s): State<Service>, Path(id): Path<Uuid>) -> Result<Json<Value>> {
    bounded(sqlx::query_scalar("SELECT jsonb_build_object('sampled_at',o.sampled_at,'digest',o.digest,'inventory',v.inventory) FROM inventory_observations o JOIN inventory_versions v USING(node_id,digest) WHERE o.node_id=$1 ORDER BY o.sampled_at DESC LIMIT 101")
        .bind(id),&s.read,100).await
}
async fn history(
    State(s): State<Service>,
    Path(id): Path<Uuid>,
    Query(r): Query<Range>,
) -> Result<Json<Value>> {
    r.check(false)?;
    let (table, column, projection) = match r.resolution {
        Resolution::Raw => ("host_samples", "sampled_at", "to_jsonb(t)-'batch_id'"),
        Resolution::Minute => (
            "host_minutes",
            "bucket",
            "to_jsonb(t) || jsonb_build_object('cpu_avg',cpu_sum/NULLIF(cpu_samples,0),'memory_avg',memory_sum/NULLIF(memory_samples,0))",
        ),
        Resolution::Hour => (
            "host_hours",
            "bucket",
            "to_jsonb(t) || jsonb_build_object('cpu_avg',cpu_sum/NULLIF(cpu_samples,0),'memory_avg',memory_sum/NULLIF(memory_samples,0))",
        ),
    };
    let sql = format!(
        "SELECT {projection} FROM {table} t WHERE node_id=$1 AND {column}>=$2 AND {column}<$3 ORDER BY {column} LIMIT 6001"
    );
    bounded(
        sqlx::query_scalar(&sql).bind(id).bind(r.from).bind(r.to),
        &s.read,
        6000,
    )
    .await
}
async fn build_history(
    State(s): State<Service>,
    Path(id): Path<Uuid>,
    Query(r): Query<Range>,
) -> Result<Json<Value>> {
    r.check(true)?;
    let (table, column, projection) = match r.resolution {
        Resolution::Raw => ("build_samples", "sampled_at", "to_jsonb(t)-'batch_id'"),
        Resolution::Minute => (
            "build_minutes",
            "bucket",
            "to_jsonb(t) || jsonb_build_object('cpu_avg',cpu_sum/NULLIF(cpu_samples,0),'rss_avg',rss_sum/NULLIF(rss_samples,0))",
        ),
        Resolution::Hour => (
            "build_hours",
            "bucket",
            "to_jsonb(t) || jsonb_build_object('cpu_avg',cpu_sum/NULLIF(cpu_samples,0),'rss_avg',rss_sum/NULLIF(rss_samples,0))",
        ),
    };
    let sql = format!(
        "SELECT {projection} FROM {table} t WHERE attempt_id=$1 AND {column}>=$2 AND {column}<$3 ORDER BY {column} LIMIT 6001"
    );
    bounded(
        sqlx::query_scalar(&sql).bind(id).bind(r.from).bind(r.to),
        &s.read,
        6000,
    )
    .await
}
async fn processes(
    State(s): State<Service>,
    Path(id): Path<Uuid>,
    Query(r): Query<Range>,
) -> Result<Json<Value>> {
    r.check(true)?;
    if !matches!(r.resolution, Resolution::Raw) {
        return Err(ApiError::BadRequest(
            "individual process history requires raw resolution".into(),
        ));
    }
    bounded(sqlx::query_scalar("SELECT to_jsonb(p)-'batch_id' FROM process_samples p WHERE attempt_id=$1 AND sampled_at>=$2 AND sampled_at<$3 ORDER BY sampled_at,pid LIMIT 6001")
        .bind(id).bind(r.from).bind(r.to),&s.read,6000).await
}
