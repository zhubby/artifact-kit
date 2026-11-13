use super::*;
pub struct SourceMetrics {
    pub ready: i64,
    pub incomplete: i64,
    pub references: i64,
}
impl SourceStore {
    pub async fn metrics(&self) -> Result<SourceMetrics> {
        let (ready,incomplete):(i64,i64)=sqlx::query_as("SELECT count(*) FILTER(WHERE durable),count(*) FILTER(WHERE NOT durable) FROM source_snapshots").fetch_one(&self.pool).await?;
        let references: i64 = sqlx::query_scalar("SELECT count(*) FROM source_retentions")
            .fetch_one(&self.pool)
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
        sqlx::query(
            "INSERT INTO source_operations(id,input_hash) VALUES($1,$2) ON CONFLICT DO NOTHING",
        )
        .bind(operation)
        .bind(&hash)
        .execute(&self.pool)
        .await?;
        let mut lock = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,2))")
            .bind(operation.to_string())
            .execute(&mut *lock)
            .await?;
        let row = sqlx::query("SELECT input_hash,snapshot_id FROM source_operations WHERE id=$1")
            .bind(operation)
            .fetch_one(&mut *lock)
            .await?;
        ensure!(
            row.get::<String, _>("input_hash") == hash,
            "source operation ID reused with different input"
        );
        let snapshot = capture(row.get("snapshot_id")).await?;
        sqlx::query(
            "INSERT INTO source_retentions(snapshot_id,owner) VALUES($1,$2) ON CONFLICT DO NOTHING",
        )
        .bind(snapshot.id)
        .bind(format!("operation:{}", operation))
        .execute(&self.pool)
        .await?;
        lock.commit().await?;
        Ok(snapshot)
    }

    pub async fn record_operation_snapshot(&self, operation: Uuid, snapshot: Uuid) -> Result<()> {
        sqlx::query("UPDATE source_operations SET snapshot_id=$2 WHERE id=$1")
            .bind(operation)
            .bind(snapshot)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn durable_digest(&self, snapshot: Uuid) -> Result<String> {
        let digest: String =
            sqlx::query_scalar("SELECT digest FROM source_snapshots WHERE id=$1 AND durable")
                .bind(snapshot)
                .fetch_one(&self.pool)
                .await?;
        Ok(digest)
    }

    pub async fn repository_metadata(&self, ids: Vec<Uuid>) -> Result<Vec<Value>> {
        let repositories:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'repository_url',repository_url,'cache_key',cache_key) FROM source_repositories WHERE id=ANY($1)").bind(ids).fetch_all(&self.pool).await?;
        Ok(repositories)
    }

    pub async fn retain_snapshot(&self, snapshot: Uuid, owner: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO source_retentions(snapshot_id,owner) VALUES($1,$2) ON CONFLICT DO NOTHING",
        )
        .bind(snapshot)
        .bind(owner)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn snapshot_durable(&self, snapshot: Uuid) -> Result<bool> {
        let durable: bool = sqlx::query_scalar("SELECT durable FROM source_snapshots WHERE id=$1")
            .bind(snapshot)
            .fetch_one(&self.pool)
            .await?;
        Ok(durable)
    }

    pub async fn mark_snapshot_durable(&self, snapshot: Uuid) -> Result<()> {
        sqlx::query("UPDATE source_snapshots SET durable=true WHERE id=$1")
            .bind(snapshot)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn save_credential(
        &self,
        id: Uuid,
        revision: &str,
        ciphertext: String,
    ) -> Result<String> {
        sqlx::query("INSERT INTO credentials(id,revision,ciphertext) VALUES($1,$2,$3) ON CONFLICT(id) DO NOTHING").bind(id).bind(revision).bind(ciphertext).execute(&self.pool).await?;
        self.credential_revision(id).await
    }
    pub async fn persist_repository<F, Fut>(&self, key: &str, persist: F) -> Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let mut lock = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtextextended($1,0))")
            .bind(key)
            .execute(&mut *lock)
            .await?;
        persist().await?;
        lock.commit().await?;
        Ok(())
    }
}
