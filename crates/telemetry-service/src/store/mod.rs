//! Telemetry persistence and read-only business authorization.
mod history;
mod ingest;
use crate::internal;
pub use history::{HistoryPage, LatestTelemetry};
pub use ingest::ingest;
use sqlx::PgPool;
use tonic::Status;
use uuid::Uuid;
#[derive(Clone)]
pub struct TelemetryStore {
    write: PgPool,
    read: PgPool,
}
#[derive(Clone)]
pub struct BusinessAuthorizationRepository {
    pool: PgPool,
}
impl TelemetryStore {
    pub fn new(write: PgPool, read: PgPool) -> Self {
        Self { write, read }
    }
    pub async fn ingest(
        &self,
        node: Uuid,
        batch: &artifactory_telemetry_model::Batch,
        bytes: &[u8],
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool, Status> {
        ingest(&self.write, node, batch, bytes, now).await
    }
    pub async fn retain(&self) -> anyhow::Result<()> {
        let mut failure = None;
        for statement in [
            "DELETE FROM telemetry_batches WHERE received_at<now()-interval '3 days'",
            "DELETE FROM inventory_observations WHERE sampled_at<now()-interval '180 days'",
            "DELETE FROM inventory_versions v WHERE NOT EXISTS(SELECT 1 FROM inventory_observations o WHERE o.node_id=v.node_id AND o.digest=v.digest) AND NOT EXISTS(SELECT 1 FROM latest_inventories l WHERE l.node_id=v.node_id AND l.digest=v.digest)",
        ] {
            if let Err(error) = sqlx::query(statement).execute(&self.write).await {
                tracing::warn!(%error, "telemetry retention failed");
                failure = Some(error);
            }
        }
        match failure {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }
}
impl BusinessAuthorizationRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
    pub async fn identify(&self, certificate_hash: String) -> Result<Uuid, Status> {
        sqlx::query_scalar("SELECT id FROM nodes WHERE certificate_hash=$1 AND NOT revoked AND certificate_expires_at>now()")
            .bind(certificate_hash).fetch_optional(&self.pool).await.map_err(internal)?.ok_or_else(|| Status::unauthenticated("Emissary certificate revoked, expired or unknown"))
    }
    pub async fn authorize_attempts(&self, node: Uuid, attempts: &[Uuid]) -> Result<(), Status> {
        if attempts.is_empty() {
            return Ok(());
        }
        let owned: i64 =
            sqlx::query_scalar("SELECT count(*) FROM attempts WHERE node_id=$1 AND id=ANY($2)")
                .bind(node)
                .bind(attempts)
                .fetch_one(&self.pool)
                .await
                .map_err(internal)?;
        if owned as usize != attempts.len() {
            return Err(Status::permission_denied(
                "process telemetry must belong to this Emissary's attempts",
            ));
        }
        Ok(())
    }
}

mod connect;
pub use connect::connect;
