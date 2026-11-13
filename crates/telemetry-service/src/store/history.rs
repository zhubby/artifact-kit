//! Bounded historical reads through the independent telemetry read pool.
use super::TelemetryStore;
use crate::query::{Range, Resolution};
use artifactory_runtime_protocol::error::{ApiError, Result};
use futures::TryStreamExt;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;
#[derive(Serialize)]
pub struct HistoryPage {
    pub samples: Vec<Value>,
    pub truncated: bool,
}
#[derive(Serialize)]
pub struct LatestTelemetry {
    pub host: Option<Value>,
    pub inventory: Option<Value>,
}
async fn bounded(
    query: sqlx::query::QueryScalar<'_, sqlx::Postgres, Value, sqlx::postgres::PgArguments>,
    pool: &sqlx::PgPool,
    limit: usize,
) -> Result<HistoryPage> {
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
    Ok(HistoryPage { samples, truncated })
}

impl TelemetryStore {
    pub async fn latest(&self, id: Uuid) -> Result<LatestTelemetry> {
        let host:Option<Value>=sqlx::query_scalar("SELECT to_jsonb(h) || jsonb_build_object('fresh', sampled_at BETWEEN now()-interval '90 seconds' AND now()+interval '30 seconds' AND received_at>now()-interval '90 seconds') FROM latest_hosts h WHERE node_id=$1")
        .bind(id).fetch_optional(&self.read).await?;
        let inventory:Option<Value>=sqlx::query_scalar("SELECT jsonb_build_object('sampled_at',i.sampled_at,'digest',i.digest,'inventory',v.inventory) FROM latest_inventories i JOIN inventory_versions v USING(node_id,digest) WHERE i.node_id=$1")
        .bind(id).fetch_optional(&self.read).await?;
        Ok(LatestTelemetry { host, inventory })
    }

    pub async fn inventory(&self, id: Uuid) -> Result<HistoryPage> {
        bounded(sqlx::query_scalar("SELECT jsonb_build_object('sampled_at',o.sampled_at,'digest',o.digest,'inventory',v.inventory) FROM inventory_observations o JOIN inventory_versions v USING(node_id,digest) WHERE o.node_id=$1 ORDER BY o.sampled_at DESC LIMIT 101")
        .bind(id),&self.read,100).await
    }

    pub async fn history(&self, id: Uuid, r: &Range) -> Result<HistoryPage> {
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
            &self.read,
            6000,
        )
        .await
    }

    pub async fn build_history(&self, id: Uuid, r: &Range) -> Result<HistoryPage> {
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
            &self.read,
            6000,
        )
        .await
    }

    pub async fn processes(&self, id: Uuid, r: &Range) -> Result<HistoryPage> {
        if !matches!(r.resolution, Resolution::Raw) {
            return Err(ApiError::BadRequest(
                "individual process history requires raw resolution".into(),
            ));
        }
        bounded(sqlx::query_scalar("SELECT to_jsonb(p)-'batch_id' FROM process_samples p WHERE attempt_id=$1 AND sampled_at>=$2 AND sampled_at<$3 ORDER BY sampled_at,pid LIMIT 6001")
        .bind(id).bind(r.from).bind(r.to),&self.read,6000).await
    }
}
