//! Source persistence and coordination. No database handle escapes this module.
use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, Row};
use std::{
    future::Future,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use uuid::Uuid;

#[derive(Clone)]
pub struct SourceStore {
    pool: PgPool,
}
pub struct RepositoryAccess {
    pub repository_url: String,
    pub credential_id: Option<Uuid>,
    pub cache_key: String,
}
pub struct FetchObservation {
    pub path: PathBuf,
    pub commit: String,
    pub fetched_bytes: Option<i64>,
    pub existing: bool,
}
pub struct FetchRequest<'a> {
    pub project: Uuid,
    pub url: &'a str,
    pub auth_id: Option<Uuid>,
    pub key: &'a str,
    pub reference: &'a str,
    pub started: DateTime<Utc>,
}
/// A download keeps its eviction protection until the response stream is dropped.
pub struct SourceDownload {
    pub bytes: Arc<AtomicU64>,
    _finished: tokio::sync::oneshot::Sender<()>,
}

impl SourceStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
    pub async fn manifest(&self, id: Uuid) -> Result<Value> {
        Ok(
            sqlx::query_scalar("SELECT manifest FROM source_snapshots WHERE id=$1")
                .bind(id)
                .fetch_one(&self.pool)
                .await?,
        )
    }
    pub async fn cache_key(&self, id: Uuid) -> Result<String> {
        Ok(
            sqlx::query_scalar("SELECT cache_key FROM source_repositories WHERE id=$1")
                .bind(id)
                .fetch_one(&self.pool)
                .await?,
        )
    }
    pub async fn credential_ciphertext(&self, id: Uuid) -> Result<String> {
        Ok(
            sqlx::query_scalar("SELECT ciphertext FROM credentials WHERE id=$1")
                .bind(id)
                .fetch_one(&self.pool)
                .await?,
        )
    }
    pub async fn credential_revision(&self, id: Uuid) -> Result<String> {
        Ok(
            sqlx::query_scalar("SELECT revision FROM credentials WHERE id=$1")
                .bind(id)
                .fetch_one(&self.pool)
                .await?,
        )
    }
    pub async fn fetch_repository<F, Fut>(
        &self,
        request: FetchRequest<'_>,
        fetch: F,
    ) -> Result<(Uuid, PathBuf, String)>
    where
        F: FnOnce(Option<String>) -> Fut,
        Fut: Future<Output = Result<FetchObservation>>,
    {
        let FetchRequest {
            project,
            url,
            auth_id,
            key,
            reference,
            started,
        } = request;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(key)
            .execute(&mut *tx)
            .await?;
        let id: Uuid = sqlx::query_scalar("INSERT INTO source_repositories(id,project_id,repository_url,credential_id,cache_key) VALUES($1,$2,$3,$4,$5) ON CONFLICT(cache_key) DO UPDATE SET last_used_at=now() RETURNING id")
            .bind(Uuid::new_v4()).bind(project).bind(url).bind(auth_id).bind(key).fetch_one(&mut *tx).await?;
        let coalesced = sqlx::query_scalar("SELECT commit_sha FROM source_resolutions WHERE repository_id=$1 AND reference=$2 AND fetched_at>=$3").bind(id).bind(reference).bind(started).fetch_optional(&mut *tx).await?;
        let result = fetch(coalesced).await?;
        if let Some(bytes) = result.fetched_bytes {
            sqlx::query("UPDATE source_repositories SET fetched_bytes=fetched_bytes+$2,cache_hits=cache_hits+CASE WHEN $2=0 THEN 1 ELSE 0 END WHERE id=$1").bind(id).bind(bytes).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO source_resolutions(repository_id,reference,commit_sha) VALUES($1,$2,$3) ON CONFLICT(repository_id,reference) DO UPDATE SET commit_sha=$3,fetched_at=clock_timestamp()")
                .bind(id).bind(reference).bind(&result.commit).execute(&mut *tx).await?;
        }
        if result.existing {
            sqlx::query("UPDATE source_repositories SET cache_hits=cache_hits+1 WHERE id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok((id, result.path, result.commit))
    }
    pub async fn capture_sources<T, F, Fut>(&self, project: Uuid, capture: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut protection = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtextextended($1,1))")
            .bind(project.to_string())
            .execute(&mut *protection)
            .await?;
        let result = capture().await?;
        protection.commit().await?;
        Ok(result)
    }
    pub async fn save_snapshot(
        &self,
        id: Uuid,
        project: Uuid,
        root: Uuid,
        commit: &str,
        manifest: Value,
        digest: String,
    ) -> Result<Value> {
        Ok(sqlx::query_scalar("INSERT INTO source_snapshots(id,project_id,root_repository_id,commit_sha,manifest,digest) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(project_id,digest) DO UPDATE SET last_used_at=now() RETURNING manifest")
            .bind(id).bind(project).bind(root).bind(commit).bind(manifest).bind(digest).fetch_one(&self.pool).await?)
    }
    pub async fn restore_repository<T, F, Fut>(&self, id: Uuid, restore: F) -> Result<T>
    where
        F: FnOnce(RepositoryAccess) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let row = sqlx::query(
            "SELECT repository_url,credential_id,cache_key FROM source_repositories WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        let access = RepositoryAccess {
            repository_url: row.get("repository_url"),
            credential_id: row.get("credential_id"),
            cache_key: row.get("cache_key"),
        };
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(&access.cache_key)
            .execute(&mut *tx)
            .await?;
        let result = restore(access).await?;
        tx.commit().await?;
        Ok(result)
    }
    pub async fn touch_snapshot(&self, id: Uuid) -> Result<()> {
        sqlx::query("UPDATE source_snapshots SET last_used_at=now() WHERE id=$1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn download_repository(&self, repository: Uuid, lfs: bool) -> Result<SourceDownload> {
        let store = self.clone();
        let bytes = Arc::new(AtomicU64::new(0));
        let measured = bytes.clone();
        let (ready, started) = tokio::sync::oneshot::channel();
        let (finished, complete) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let protected = async {
                let key = store.cache_key(repository).await?;
                let mut tx = store.pool.begin().await?;
                sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtextextended($1,0))")
                    .bind(key)
                    .execute(&mut *tx)
                    .await?;
                Ok::<_, anyhow::Error>(tx)
            }
            .await;
            match protected {
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
                Ok(lock) => {
                    if ready.send(Ok(())).is_err() {
                        return;
                    }
                    let _ = complete.await;
                    drop(lock);
                    let _ = store
                        .record_download(repository, measured.load(Ordering::Relaxed) as i64, lfs)
                        .await;
                }
            }
        });
        started.await??;
        Ok(SourceDownload {
            bytes,
            _finished: finished,
        })
    }
    async fn record_download(&self, id: Uuid, bytes: i64, lfs: bool) -> Result<()> {
        sqlx::query("UPDATE source_repositories SET git_download_bytes=git_download_bytes+CASE WHEN $3 THEN 0 ELSE $2 END,lfs_download_bytes=lfs_download_bytes+CASE WHEN $3 THEN $2 ELSE 0 END,last_used_at=now() WHERE id=$1").bind(id).bind(bytes).bind(lfs).execute(&self.pool).await?;
        Ok(())
    }
}

impl SourceStore {
    pub async fn pinned_root(&self, operation: Uuid) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT root_commit FROM source_operations WHERE id=$1")
                .bind(operation)
                .fetch_one(&self.pool)
                .await?,
        )
    }
    pub async fn pin_root(&self, operation: Uuid, commit: &str) -> Result<()> {
        sqlx::query(
            "UPDATE source_operations SET root_commit=coalesce(root_commit,$2) WHERE id=$1",
        )
        .bind(operation)
        .bind(commit)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    pub async fn frozen_tags(&self, operation: Uuid, path: &str) -> Result<Option<Value>> {
        Ok(sqlx::query_scalar(
            "SELECT tags FROM source_operation_repos WHERE operation_id=$1 AND path=$2",
        )
        .bind(operation)
        .bind(path)
        .fetch_optional(&self.pool)
        .await?)
    }
    pub async fn freeze_tags(&self, operation: Uuid, path: &str, tags: Value) -> Result<()> {
        sqlx::query("INSERT INTO source_operation_repos(operation_id,path,tags) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(operation).bind(path).bind(tags).execute(&self.pool).await?;
        Ok(())
    }
}

mod service;
pub use service::SourceMetrics;
