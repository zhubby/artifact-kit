//! Build-aware source retention and cache accounting.
use super::ControlStore;
use anyhow::Result;
use artifactory_database::{params, sql};
use serde_json::json;
use toasty::stmt::Type;

use uuid::Uuid;
pub(crate) struct SourceCacheEntry {
    pub id: Uuid,
    pub project: Uuid,
    pub key: String,
}
impl ControlStore {
    pub(crate) async fn source_cache_entries(&self) -> Result<Vec<SourceCacheEntry>> {
        let rows = sql::records(
            &mut self.pool.clone(),
            "SELECT id,project_id,cache_key FROM source_repositories ORDER BY last_used_at",
            params![],
            &[
                ("id", Type::Uuid),
                ("project_id", Type::Uuid),
                ("cache_key", Type::String),
            ],
        )
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                Ok::<_, artifactory_database::Error>(SourceCacheEntry {
                    id: row.decode("id")?,
                    project: row.decode("project_id")?,
                    key: row.decode("cache_key")?,
                })
            })
            .collect::<artifactory_database::Result<_>>()?)
    }
    pub(crate) async fn source_cache_measured(&self, id: Uuid, bytes: u64) -> Result<()> {
        sql::execute(
            &mut self.pool.clone(),
            "UPDATE source_repositories SET cache_bytes=$2 WHERE id=$1",
            params![id, bytes as i64],
        )
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
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        let project: bool = sql::scalar(
            &mut tx,
            "SELECT pg_try_advisory_xact_lock(hashtextextended($1,1))",
            params![row.project.to_string()],
        )
        .await?;
        if !project {
            return Ok(false);
        }
        let idle: bool = sql::scalar(
            &mut tx,
            "SELECT pg_try_advisory_xact_lock(hashtextextended($1,0))",
            params![&row.key],
        )
        .await?;
        if !idle {
            return Ok(false);
        }
        let protected:bool=sql::scalar(&mut tx, "SELECT EXISTS(SELECT 1 FROM builds b JOIN source_snapshots s ON s.id=b.source_snapshot_id WHERE b.status NOT IN ('succeeded','failed','lost','canceled') AND s.manifest->'repositories' @> $1::jsonb)", params![json!([{"repository_id":id}])]).await?;
        if protected {
            return Ok(false);
        }
        remove().await?;
        sql::execute(
            &mut tx,
            "UPDATE source_repositories SET cache_bytes=0 WHERE id=$1",
            params![id],
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }
    pub(crate) async fn expire_source_tokens(&self) -> Result<()> {
        sql::execute(
            &mut self.pool.clone(),
            "DELETE FROM source_tokens WHERE expires_at<now()-interval '1 day'",
            params![],
        )
        .await?;
        Ok(())
    }
    pub(crate) async fn authorize_source_attempt(
        &self,
        token_hash: String,
        snapshot_id: Uuid,
        attempt: Uuid,
    ) -> Result<()> {
        sql::execute(&mut self.pool.clone(), "INSERT INTO source_tokens(token_hash,snapshot_id,attempt_id,expires_at) VALUES($1,$2,$3,now()+interval '1 day')", params![token_hash, snapshot_id, attempt]).await?;
        Ok(())
    }
}
