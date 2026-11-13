//! Atomic, idempotent batch persistence and monotonic latest summaries.
use crate::internal;
use artifactory_runtime_protocol::crypto;
use artifactory_telemetry_model::Batch;
use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::{PgPool, Postgres, QueryBuilder};
use std::collections::BTreeMap;
use tonic::Status;
use uuid::Uuid;

/// Commit all rows before ACK. A reused batch ID with different bytes is rejected.
pub async fn ingest(
    pool: &PgPool,
    node: Uuid,
    b: &Batch,
    bytes: &[u8],
    received: DateTime<Utc>,
) -> Result<bool, Status> {
    let mut tx = pool.begin().await.map_err(internal)?;
    let digest = crypto::hash(bytes);
    let inserted=sqlx::query("INSERT INTO telemetry_batches(node_id,batch_id,digest,received_at) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING")
        .bind(node).bind(b.id).bind(&digest).bind(received).execute(&mut *tx).await.map_err(internal)?.rows_affected();
    if inserted == 0 {
        let previous: String = sqlx::query_scalar(
            "SELECT digest FROM telemetry_batches WHERE node_id=$1 AND batch_id=$2",
        )
        .bind(node)
        .bind(b.id)
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?;
        if previous != digest {
            return Err(Status::already_exists(
                "batch ID reused with different content",
            ));
        }
        tx.commit().await.map_err(internal)?;
        return Ok(true);
    }
    if let Some(h) = &b.host {
        sqlx::query("INSERT INTO host_samples(sampled_at,node_id,batch_id,session_id,monotonic_ms,received_at,cpu_percent,memory_total_bytes,memory_used_bytes,swap_total_bytes,swap_used_bytes,load_one,active_attempts,dropped_batches,devices) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)")
            .bind(b.sampled_at).bind(node).bind(b.id).bind(b.session_id).bind(b.monotonic_ms as i64).bind(received)
            .bind(h.cpu_percent).bind(h.memory_total_bytes.map(|n|n as i64)).bind(h.memory_used_bytes.map(|n|n as i64))
            .bind(h.swap_total_bytes.map(|n|n as i64)).bind(h.swap_used_bytes.map(|n|n as i64)).bind(h.load_one)
            .bind(h.active_attempts as i32).bind(b.dropped_batches as i64)
            .bind(json!({"disks":h.disks,"networks":h.networks,"accelerators":h.accelerators}))
            .execute(&mut *tx).await.map_err(internal)?;
        sqlx::query("INSERT INTO latest_hosts(node_id,sampled_at,received_at,session_id,monotonic_ms,dropped_batches,sample) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(node_id) DO UPDATE SET sampled_at=excluded.sampled_at,received_at=excluded.received_at,session_id=excluded.session_id,monotonic_ms=excluded.monotonic_ms,dropped_batches=excluded.dropped_batches,sample=excluded.sample WHERE excluded.sampled_at>latest_hosts.sampled_at")
            .bind(node).bind(b.sampled_at).bind(received).bind(b.session_id).bind(b.monotonic_ms as i64).bind(b.dropped_batches as i64)
            .bind(serde_json::to_value(h).map_err(internal)?).execute(&mut *tx).await.map_err(internal)?;
    }
    if !b.processes.is_empty() {
        let mut query: QueryBuilder<Postgres> = QueryBuilder::new(
            "INSERT INTO process_samples(sampled_at,node_id,batch_id,attempt_id,pid,started_at,parent_pid,step,cpu_percent,rss_bytes,read_bytes,written_bytes,status) ",
        );
        query.push_values(&b.processes, |mut row, p| {
            row.push_bind(b.sampled_at)
                .push_bind(node)
                .push_bind(b.id)
                .push_bind(p.attempt_id)
                .push_bind(i64::from(p.pid))
                .push_bind(p.started_at as i64)
                .push_bind(p.parent_pid.map(i64::from))
                .push_bind(p.step.map(i64::from))
                .push_bind(p.cpu_percent)
                .push_bind(p.rss_bytes.map(|n| n as i64))
                .push_bind(p.read_bytes.map(|n| n as i64))
                .push_bind(p.written_bytes.map(|n| n as i64))
                .push_bind(&p.status);
        });
        query.build().execute(&mut *tx).await.map_err(internal)?;
        let mut grouped = BTreeMap::<_, Vec<_>>::new();
        for p in &b.processes {
            grouped.entry(p.attempt_id).or_default().push(p);
        }
        let mut query: QueryBuilder<Postgres> = QueryBuilder::new(
            "INSERT INTO build_samples(sampled_at,node_id,batch_id,attempt_id,cpu_percent,rss_bytes,read_bytes,written_bytes,process_count,complete) ",
        );
        query.push_values(grouped, |mut row, (attempt, ps)| {
            // Missing data in any process makes that total unknown; truncation is reported separately.
            let cpu: Option<f64> = ps.iter().map(|p| p.cpu_percent).sum();
            let sum = |f: fn(&artifactory_telemetry_model::ProcessSample) -> Option<u64>| {
                ps.iter().try_fold(0i64, |n, p| n.checked_add(f(p)? as i64))
            };
            row.push_bind(b.sampled_at)
                .push_bind(node)
                .push_bind(b.id)
                .push_bind(attempt)
                .push_bind(cpu)
                .push_bind(sum(|p| p.rss_bytes))
                .push_bind(sum(|p| p.read_bytes))
                .push_bind(sum(|p| p.written_bytes))
                .push_bind(ps.len() as i32)
                .push_bind(b.omitted_processes == 0);
        });
        query.build().execute(&mut *tx).await.map_err(internal)?;
    }
    if let Some(inventory) = &b.inventory {
        let value = serde_json::to_value(inventory).map_err(internal)?;
        let fingerprint = crypto::hash(serde_json::to_vec(&value).map_err(internal)?);
        sqlx::query("INSERT INTO inventory_versions(node_id,digest,first_seen,inventory) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING")
            .bind(node).bind(&fingerprint).bind(b.sampled_at).bind(value).execute(&mut *tx).await.map_err(internal)?;
        sqlx::query("INSERT INTO inventory_observations(node_id,sampled_at,digest) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(node).bind(b.sampled_at).bind(&fingerprint).execute(&mut *tx).await.map_err(internal)?;
        sqlx::query("INSERT INTO latest_inventories(node_id,sampled_at,digest) VALUES($1,$2,$3) ON CONFLICT(node_id) DO UPDATE SET sampled_at=excluded.sampled_at,digest=excluded.digest WHERE excluded.sampled_at>latest_inventories.sampled_at")
            .bind(node).bind(b.sampled_at).bind(fingerprint).execute(&mut *tx).await.map_err(internal)?;
    }
    tx.commit().await.map_err(internal)?;
    Ok(false)
}
