use super::*;
use artifactory_database::{params, sql};
use toasty::stmt::Type;
pub struct SourceMetrics {
    pub ready: i64,
    pub incomplete: i64,
    pub references: i64,
}
impl SourceStore {
    pub async fn metrics(&self) -> Result<SourceMetrics> {
        let (ready,incomplete):(i64,i64)=sql::tuple(&mut self.pool.clone(), "SELECT count(*) FILTER(WHERE durable),count(*) FILTER(WHERE NOT durable) FROM source_snapshots", params![]).await?;
        let references: i64 = sql::scalar(
            &mut self.pool.clone(),
            "SELECT count(*) FROM source_retentions",
            params![],
        )
        .await?;
        Ok(SourceMetrics {
            ready,
            incomplete,
            references,
        })
    }

    pub async fn capture_operation<F, Fut>(
        &self,
        operation: Uuid,
        hash: String,
        capture: F,
    ) -> Result<crate::Snapshot>
    where
        F: FnOnce(Option<Uuid>) -> Fut,
        Fut: Future<Output = Result<crate::Snapshot>>,
    {
        models::Operation::upsert_by_id(operation)
            .input_hash(&hash)
            .or_ignore()
            .exec(&mut self.pool.clone())
            .await?;
        let mut lock_db = self.pool.clone();
        let mut lock = lock_db.transaction().await?;
        sql::execute(
            &mut lock,
            "SELECT pg_advisory_xact_lock(hashtextextended($1,2))",
            params![operation.to_string()],
        )
        .await?;
        let row = sql::record(
            &mut lock,
            "SELECT input_hash,snapshot_id FROM source_operations WHERE id=$1",
            params![operation],
            &[("input_hash", Type::String), ("snapshot_id", Type::Uuid)],
        )
        .await?;
        ensure!(
            row.decode::<String>("input_hash")? == hash,
            "source operation ID reused with different input"
        );
        let snapshot = capture(row.decode("snapshot_id")?).await?;
        models::Retention::upsert_by_snapshot_id_and_owner(
            snapshot.id,
            format!("operation:{}", operation),
        )
        .or_ignore()
        .exec(&mut self.pool.clone())
        .await?;
        lock.commit().await?;
        Ok(snapshot)
    }

    pub async fn record_operation_snapshot(&self, operation: Uuid, snapshot: Uuid) -> Result<()> {
        models::Operation::filter_by_id(operation)
            .update()
            .snapshot_id(Some(snapshot))
            .exec(&mut self.pool.clone())
            .await?;
        Ok(())
    }

    pub async fn durable_digest(&self, snapshot: Uuid) -> Result<String> {
        let digest: String = sql::scalar(
            &mut self.pool.clone(),
            "SELECT digest FROM source_snapshots WHERE id=$1 AND durable",
            params![snapshot],
        )
        .await?;
        Ok(digest)
    }

    pub async fn repository_metadata(&self, ids: Vec<Uuid>) -> Result<Vec<Value>> {
        let repositories:Vec<Value>=sql::scalars(&mut self.pool.clone(), "SELECT jsonb_build_object('id',id,'repository_url',repository_url,'cache_key',cache_key) FROM source_repositories WHERE id=ANY($1)", params![ids]).await?;
        Ok(repositories)
    }

    pub async fn retain_snapshot(&self, snapshot: Uuid, owner: &str) -> Result<()> {
        models::Retention::upsert_by_snapshot_id_and_owner(snapshot, owner)
            .or_ignore()
            .exec(&mut self.pool.clone())
            .await?;
        Ok(())
    }

    pub async fn snapshot_durable(&self, snapshot: Uuid) -> Result<bool> {
        let durable: bool = models::Snapshot::filter_by_id(snapshot)
            .select(models::Snapshot::fields().durable())
            .one()
            .exec(&mut self.pool.clone())
            .await?;
        Ok(durable)
    }

    pub async fn mark_snapshot_durable(&self, snapshot: Uuid) -> Result<()> {
        models::Snapshot::filter_by_id(snapshot)
            .update()
            .durable(true)
            .exec(&mut self.pool.clone())
            .await?;
        Ok(())
    }
    pub async fn save_credential(
        &self,
        id: Uuid,
        revision: &str,
        ciphertext: String,
    ) -> Result<String> {
        models::ServiceCredential::upsert_by_id(id)
            .revision(revision)
            .ciphertext(ciphertext)
            .or_ignore()
            .exec(&mut self.pool.clone())
            .await?;
        self.credential_revision(id).await
    }
    pub async fn persist_repository<F, Fut>(&self, key: &str, persist: F) -> Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let mut lock_db = self.pool.clone();
        let mut lock = lock_db.transaction().await?;
        sql::execute(
            &mut lock,
            "SELECT pg_advisory_xact_lock_shared(hashtextextended($1,0))",
            params![key],
        )
        .await?;
        persist().await?;
        lock.commit().await?;
        Ok(())
    }
}
