//! Atomic, idempotent batch persistence and monotonic latest summaries.
use super::models;
use crate::internal;
use artifactory_database::{params, sql};
use artifactory_runtime_protocol::crypto;
use artifactory_telemetry_model::Batch;
use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::BTreeMap;
use toasty::Db;
use tonic::Status;
use uuid::Uuid;

/// Commit all rows before ACK. A reused batch ID with different bytes is rejected.
pub async fn ingest(
    pool: &Db,
    node: Uuid,
    b: &Batch,
    bytes: &[u8],
    received: DateTime<Utc>,
) -> Result<bool, Status> {
    let mut tx_db = pool.clone();
    let mut tx = tx_db.transaction().await.map_err(internal)?;
    let digest = crypto::hash(bytes);
    let inserted = models::TelemetryBatch::upsert_by_node_id_and_batch_id(node, b.id)
        .digest(&digest)
        .received_at(sql::timestamp(received).map_err(internal)?)
        .or_ignore()
        .exec(&mut tx)
        .await
        .map_err(internal)?;
    if inserted.is_none() {
        let previous = models::TelemetryBatch::get_by_node_id_and_batch_id(&mut tx, node, b.id)
            .await
            .map_err(internal)?
            .digest;
        if previous != digest {
            return Err(Status::already_exists(
                "batch ID reused with different content",
            ));
        }
        tx.commit().await.map_err(internal)?;
        return Ok(true);
    }
    if let Some(h) = &b.host {
        sql::execute(&mut tx, "INSERT INTO host_samples(sampled_at,node_id,batch_id,session_id,monotonic_ms,received_at,cpu_percent,memory_total_bytes,memory_used_bytes,swap_total_bytes,swap_used_bytes,load_one,active_attempts,dropped_batches,devices) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)", params![b.sampled_at, node, b.id, b.session_id, b.monotonic_ms as i64, received, h.cpu_percent, h.memory_total_bytes.map(|n|n as i64), h.memory_used_bytes.map(|n|n as i64), h.swap_total_bytes.map(|n|n as i64), h.swap_used_bytes.map(|n|n as i64), h.load_one, h.active_attempts as i32, b.dropped_batches as i64, json!({"disks":h.disks,"networks":h.networks,"accelerators":h.accelerators})]).await.map_err(internal)?;
        sql::execute(&mut tx, "INSERT INTO latest_hosts(node_id,sampled_at,received_at,session_id,monotonic_ms,dropped_batches,sample) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(node_id) DO UPDATE SET sampled_at=excluded.sampled_at,received_at=excluded.received_at,session_id=excluded.session_id,monotonic_ms=excluded.monotonic_ms,dropped_batches=excluded.dropped_batches,sample=excluded.sample WHERE excluded.sampled_at>latest_hosts.sampled_at", params![node, b.sampled_at, received, b.session_id, b.monotonic_ms as i64, b.dropped_batches as i64, serde_json::to_value(h).map_err(internal)?]).await.map_err(internal)?;
    }
    if !b.processes.is_empty() {
        let mut statement = String::from(
            "INSERT INTO process_samples(sampled_at,node_id,batch_id,attempt_id,pid,started_at,parent_pid,step,cpu_percent,rss_bytes,read_bytes,written_bytes,status) VALUES ",
        );
        let mut parameters = Vec::new();
        for (index, p) in b.processes.iter().enumerate() {
            append_values(&mut statement, index, 13);
            parameters.extend(params![
                b.sampled_at,
                node,
                b.id,
                p.attempt_id,
                i64::from(p.pid),
                p.started_at as i64,
                p.parent_pid.map(i64::from),
                p.step.map(i64::from),
                p.cpu_percent,
                p.rss_bytes.map(|n| n as i64),
                p.read_bytes.map(|n| n as i64),
                p.written_bytes.map(|n| n as i64),
                p.status
            ]);
        }
        sql::execute(&mut tx, &statement, parameters)
            .await
            .map_err(internal)?;
        let mut grouped = BTreeMap::<_, Vec<_>>::new();
        for p in &b.processes {
            grouped.entry(p.attempt_id).or_default().push(p);
        }
        let mut statement = String::from(
            "INSERT INTO build_samples(sampled_at,node_id,batch_id,attempt_id,cpu_percent,rss_bytes,read_bytes,written_bytes,process_count,complete) VALUES ",
        );
        let mut parameters = Vec::new();
        for (index, (attempt, ps)) in grouped.into_iter().enumerate() {
            // Missing process data keeps the aggregate unknown.
            let cpu: Option<f64> = ps.iter().map(|p| p.cpu_percent).sum();
            let sum = |f: fn(&artifactory_telemetry_model::ProcessSample) -> Option<u64>| {
                ps.iter().try_fold(0i64, |n, p| n.checked_add(f(p)? as i64))
            };
            append_values(&mut statement, index, 10);
            parameters.extend(params![
                b.sampled_at,
                node,
                b.id,
                attempt,
                cpu,
                sum(|p| p.rss_bytes),
                sum(|p| p.read_bytes),
                sum(|p| p.written_bytes),
                ps.len() as i32,
                b.omitted_processes == 0
            ]);
        }
        sql::execute(&mut tx, &statement, parameters)
            .await
            .map_err(internal)?;
    }
    if let Some(inventory) = &b.inventory {
        let value = serde_json::to_value(inventory).map_err(internal)?;
        let fingerprint = crypto::hash(serde_json::to_vec(&value).map_err(internal)?);
        models::InventoryVersion::upsert_by_node_id_and_digest(node, &fingerprint)
            .first_seen(sql::timestamp(b.sampled_at).map_err(internal)?)
            .inventory(toasty::Json(value))
            .or_ignore()
            .exec(&mut tx)
            .await
            .map_err(internal)?;
        models::InventoryObservation::upsert_by_node_id_and_sampled_at(
            node,
            sql::timestamp(b.sampled_at).map_err(internal)?,
        )
        .digest(&fingerprint)
        .or_ignore()
        .exec(&mut tx)
        .await
        .map_err(internal)?;
        sql::execute(&mut tx, "INSERT INTO latest_inventories(node_id,sampled_at,digest) VALUES($1,$2,$3) ON CONFLICT(node_id) DO UPDATE SET sampled_at=excluded.sampled_at,digest=excluded.digest WHERE excluded.sampled_at>latest_inventories.sampled_at", params![node, b.sampled_at, fingerprint]).await.map_err(internal)?;
    }
    tx.commit().await.map_err(internal)?;
    Ok(false)
}

// Identifiers are fixed above; only numeric placeholder positions are generated.
fn append_values(statement: &mut String, row: usize, columns: usize) {
    if row > 0 {
        statement.push(',');
    }
    statement.push('(');
    for column in 0..columns {
        if column > 0 {
            statement.push(',');
        }
        statement.push('$');
        statement.push_str(&(row * columns + column + 1).to_string());
    }
    statement.push(')');
}
