//! Build-aware source retention and cache accounting.
use super::ControlStore;
use anyhow::Result;
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;
pub(crate) struct SourceCacheEntry {
    pub id: Uuid,
    pub project: Uuid,
    pub key: String,
}
impl ControlStore {
    pub(crate) async fn source_cache_entries(&self) -> Result<Vec<SourceCacheEntry>> {
        let rows = sqlx::query(
            "SELECT id,project_id,cache_key FROM source_repositories ORDER BY last_used_at",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| SourceCacheEntry {
                id: row.get("id"),
                project: row.get("project_id"),
                key: row.get("cache_key"),
            })
            .collect())
    }
    pub(crate) async fn source_cache_measured(&self, id: Uuid, bytes: u64) -> Result<()> {
        sqlx::query("UPDATE source_repositories SET cache_bytes=$2 WHERE id=$1")
            .bind(id)
            .bind(bytes as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub(crate) async fn evict_source_repository<F, Fut>(
        &self,
        row: &SourceCacheEntry,
        remove: F,
    ) -> Result<bool>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let id = row.id;
        let mut tx = self.pool.begin().await?;
        let project: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1,1))")
                .bind(row.project.to_string())
                .fetch_one(&mut *tx)
                .await?;
        if !project {
            return Ok(false);
        }
        let idle: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1,0))")
                .bind(&row.key)
                .fetch_one(&mut *tx)
                .await?;
        if !idle {
            return Ok(false);
        }
        let protected:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM builds b JOIN source_snapshots s ON s.id=b.source_snapshot_id WHERE b.status NOT IN ('succeeded','failed','lost','canceled') AND s.manifest->'repositories' @> $1::jsonb)").bind(json!([{"repository_id":id}])).fetch_one(&mut *tx).await?;
        if protected {
            return Ok(false);
        }
        remove().await?;
        sqlx::query("UPDATE source_repositories SET cache_bytes=0 WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }
    pub(crate) async fn expire_source_tokens(&self) -> Result<()> {
        sqlx::query("DELETE FROM source_tokens WHERE expires_at<now()-interval '1 day'")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub(crate) async fn authorize_source_attempt(
        &self,
        token_hash: String,
        snapshot_id: Uuid,
        attempt: Uuid,
    ) -> Result<()> {
        sqlx::query("INSERT INTO source_tokens(token_hash,snapshot_id,attempt_id,expires_at) VALUES($1,$2,$3,now()+interval '1 day')")
        .bind(token_hash).bind(snapshot_id).bind(attempt).execute(&self.pool).await?;
        Ok(())
    }
}
