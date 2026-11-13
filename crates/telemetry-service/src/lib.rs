//! Independent Emissary telemetry ingestion and authenticated historical queries.
pub mod query;
pub mod store;

use artifactory_emissary_protocol::{TelemetryAck, TelemetryBatch, telemetry_server::Telemetry};
use artifactory_runtime_protocol::crypto;
use artifactory_telemetry_model::{Batch, MAX_BATCH_BYTES};
use sqlx::PgPool;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use tonic::{
    Request, Response, Status,
    transport::server::{TcpConnectInfo, TlsConnectInfo},
};
use uuid::Uuid;

// Bound decoded batches waiting for database pools across all device connections.
static INGEST_LIMIT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(64);
static ACCEPTED: AtomicU64 = AtomicU64::new(0);
static REPLAYED: AtomicU64 = AtomicU64::new(0);
static REJECTED: AtomicU64 = AtomicU64::new(0);

/// Bounded-cardinality counters, without node, attempt or project labels.
pub fn metrics() -> String {
    format!(
        "{}# TYPE artifactory_telemetry_accepted_total counter\nartifactory_telemetry_accepted_total {}\n# TYPE artifactory_telemetry_replayed_total counter\nartifactory_telemetry_replayed_total {}\n# TYPE artifactory_telemetry_rejected_total counter\nartifactory_telemetry_rejected_total {}\n# TYPE artifactory_telemetry_ingest_inflight gauge\nartifactory_telemetry_ingest_inflight {}\n",
        artifactory_runtime_protocol::metrics::render(),
        ACCEPTED.load(Relaxed),
        REPLAYED.load(Relaxed),
        REJECTED.load(Relaxed),
        64 - INGEST_LIMIT.available_permits()
    )
}

/// Separate telemetry data pools and a read-only business authorization pool.
#[derive(Clone)]
pub struct Service {
    pub write: PgPool,
    pub read: PgPool,
    pub business: PgPool,
}

pub(crate) fn internal(error: impl std::fmt::Display) -> Status {
    tracing::error!(%error,"telemetry operation failed");
    Status::unavailable("telemetry temporarily unavailable")
}

#[tonic::async_trait]
impl Telemetry for Service {
    async fn upload(
        &self,
        request: Request<TelemetryBatch>,
    ) -> Result<Response<TelemetryAck>, Status> {
        let result = self.receive(request).await;
        match &result {
            Ok(ack) if ack.get_ref().duplicate => {
                REPLAYED.fetch_add(1, Relaxed);
            }
            Ok(_) => {
                ACCEPTED.fetch_add(1, Relaxed);
            }
            Err(_) => {
                REJECTED.fetch_add(1, Relaxed);
            }
        }
        result
    }
}

impl Service {
    async fn receive(
        &self,
        request: Request<TelemetryBatch>,
    ) -> Result<Response<TelemetryAck>, Status> {
        let _permit = INGEST_LIMIT.try_acquire().map_err(|_| {
            Status::resource_exhausted("telemetry ingestion is busy; retain and retry this batch")
        })?;
        let cert = request
            .extensions()
            .get::<TlsConnectInfo<TcpConnectInfo>>()
            .and_then(|info| info.peer_certs())
            .and_then(|certs| certs.first().cloned())
            .ok_or_else(|| Status::unauthenticated("Emissary certificate required"))?;
        let node:Uuid=sqlx::query_scalar("SELECT id FROM nodes WHERE certificate_hash=$1 AND NOT revoked AND certificate_expires_at>now()")
            .bind(crypto::hash(cert.as_ref())).fetch_optional(&self.business).await.map_err(internal)?
            .ok_or_else(||Status::unauthenticated("Emissary certificate revoked, expired or unknown"))?;
        let bytes = request.into_inner().json;
        if bytes.len() > MAX_BATCH_BYTES {
            return Err(Status::invalid_argument("telemetry batch too large"));
        }
        let batch: Batch = serde_json::from_slice(&bytes)
            .map_err(|_| Status::invalid_argument("invalid telemetry batch"))?;
        batch
            .validate()
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let now = chrono::Utc::now();
        if batch.sampled_at > now + chrono::Duration::minutes(5)
            || batch.sampled_at < now - chrono::Duration::hours(48)
        {
            return Err(Status::out_of_range(
                "sample clock outside the 48-hour replay window",
            ));
        }
        let attempts: Vec<Uuid> = batch
            .processes
            .iter()
            .map(|p| p.attempt_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        if !attempts.is_empty() {
            let owned: i64 =
                sqlx::query_scalar("SELECT count(*) FROM attempts WHERE node_id=$1 AND id=ANY($2)")
                    .bind(node)
                    .bind(&attempts)
                    .fetch_one(&self.business)
                    .await
                    .map_err(internal)?;
            if owned as usize != attempts.len() {
                return Err(Status::permission_denied(
                    "process telemetry must belong to this Emissary's attempts",
                ));
            }
        }
        let duplicate = store::ingest(&self.write, node, &batch, &bytes, now).await?;
        Ok(Response::new(TelemetryAck {
            batch_id: batch.id.to_string(),
            duplicate,
            server_time_ms: now.timestamp_millis(),
        }))
    }
}
