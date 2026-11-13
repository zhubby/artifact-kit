//! Telemetry persistence and read-only business authorization.
use artifactory_database::{params, sql};
mod history;
mod ingest;
mod models;
use crate::internal;
pub use history::{HistoryPage, LatestTelemetry};
pub use ingest::ingest;
use tonic::Status;
use uuid::Uuid;
#[derive(Clone)]
pub struct TelemetryStore {
    write: toasty::Db,
    read: toasty::Db,
}
#[derive(Clone)]
pub struct BusinessAuthorizationRepository {
    pool: toasty::Db,
}
impl TelemetryStore {
    pub fn new(write: toasty::Db, read: toasty::Db) -> Self {
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
        let mut db = self.write.clone();
        let mut transaction = db.transaction().await?;
        let acquired: bool = sql::scalar(&mut transaction, "SELECT pg_try_advisory_xact_lock(hashtextextended('artifactory:telemetry-retention',12))", params![]).await?;
        if !acquired {
            return Ok(());
        }
        for statement in [
            "DELETE FROM telemetry_batches WHERE received_at<now()-interval '3 days'",
            "DELETE FROM inventory_observations WHERE sampled_at<now()-interval '180 days'",
            "DELETE FROM inventory_versions v WHERE NOT EXISTS(SELECT 1 FROM inventory_observations o WHERE o.node_id=v.node_id AND o.digest=v.digest) AND NOT EXISTS(SELECT 1 FROM latest_inventories l WHERE l.node_id=v.node_id AND l.digest=v.digest)",
        ] {
            sql::execute(&mut transaction, statement, params![]).await?;
        }
        transaction.commit().await?;
        Ok(())
    }
}
impl BusinessAuthorizationRepository {
    pub fn new(pool: toasty::Db) -> Self {
        Self { pool }
    }
    pub async fn identify(&self, certificate_hash: String) -> Result<Uuid, Status> {
        sql::optional_scalar(&mut self.pool.clone(), "SELECT id FROM nodes WHERE certificate_hash=$1 AND NOT revoked AND certificate_expires_at>now()", params![certificate_hash]).await.map_err(internal)?.ok_or_else(|| Status::unauthenticated("Emissary certificate revoked, expired or unknown"))
    }
    pub async fn authorize_attempts(&self, node: Uuid, attempts: &[Uuid]) -> Result<(), Status> {
        if attempts.is_empty() {
            return Ok(());
        }
        let owned: i64 = sql::scalar(
            &mut self.pool.clone(),
            "SELECT count(*) FROM attempts WHERE node_id=$1 AND id=ANY($2)",
            params![node, attempts],
        )
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

pub static MIGRATIONS: toasty::migration::MigrationSet = toasty::embed_migrations!("migrations");

pub fn models() -> toasty::schema::app::ModelSet {
    toasty::models!(
        models::TelemetryBatch,
        models::InventoryVersion,
        models::InventoryObservation
    )
}
