//! Source persistence and coordination. No database handle escapes this module.
mod models;
use anyhow::{Result, ensure};
use artifactory_database::{params, sql};
use chrono::{DateTime, Utc};
use serde_json::Value;

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
    pool: toasty::Db,
    embedded: bool,
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
    pub fn new(pool: toasty::Db) -> Self {
        Self {
            pool,
            embedded: false,
        }
    }
    pub fn embedded(pool: toasty::Db) -> Self {
        Self {
            pool,
            embedded: true,
        }
    }
    pub async fn manifest(&self, id: Uuid) -> Result<Value> {
        Ok(models::Snapshot::filter_by_id(id)
            .select(models::Snapshot::fields().manifest())
            .one()
            .exec(&mut self.pool.clone())
            .await?
            .0)
    }
    pub async fn cache_key(&self, id: Uuid) -> Result<String> {
        Ok(models::Repository::filter_by_id(id)
            .select(models::Repository::fields().cache_key())
            .one()
            .exec(&mut self.pool.clone())
            .await?)
    }
    pub async fn credential_ciphertext(&self, id: Uuid) -> Result<String> {
        Ok(if self.embedded {
            models::ControlCredential::filter_by_id(id)
                .select(models::ControlCredential::fields().ciphertext())
                .one()
                .exec(&mut self.pool.clone())
                .await?
        } else {
            models::ServiceCredential::filter_by_id(id)
                .select(models::ServiceCredential::fields().ciphertext())
                .one()
                .exec(&mut self.pool.clone())
                .await?
        })
    }
    pub async fn credential_revision(&self, id: Uuid) -> Result<String> {
        Ok(models::ServiceCredential::filter_by_id(id)
            .select(models::ServiceCredential::fields().revision())
            .one()
            .exec(&mut self.pool.clone())
            .await?)
    }
    pub async fn create_control_credential(
        &self,
        id: Uuid,
        name: String,
        kind: String,
        ciphertext: String,
    ) -> artifactory_database::Result<()> {
        models::ControlCredential::create()
            .id(id)
            .name(name)
            .kind(kind)
            .ciphertext(ciphertext)
            .exec(&mut self.pool.clone())
            .await?;
        Ok(())
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
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        sql::execute(
            &mut tx,
            "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
            params![key],
        )
        .await?;
        let id: Uuid = sql::scalar(&mut tx, "INSERT INTO source_repositories(id,project_id,repository_url,credential_id,cache_key) VALUES($1,$2,$3,$4,$5) ON CONFLICT(cache_key) DO UPDATE SET last_used_at=now() RETURNING id", params![Uuid::new_v4(), project, url, auth_id, key]).await?;
        let coalesced = sql::optional_scalar(&mut tx, "SELECT commit_sha FROM source_resolutions WHERE repository_id=$1 AND reference=$2 AND fetched_at>=$3", params![id, reference, started]).await?;
        let result = fetch(coalesced).await?;
        if let Some(bytes) = result.fetched_bytes {
            sql::execute(&mut tx, "UPDATE source_repositories SET fetched_bytes=fetched_bytes+$2,cache_hits=cache_hits+CASE WHEN $2=0 THEN 1 ELSE 0 END WHERE id=$1", params![id, bytes]).await?;
            sql::execute(&mut tx, "INSERT INTO source_resolutions(repository_id,reference,commit_sha) VALUES($1,$2,$3) ON CONFLICT(repository_id,reference) DO UPDATE SET commit_sha=$3,fetched_at=clock_timestamp()", params![id, reference, &result.commit]).await?;
        }
        if result.existing {
            sql::execute(
                &mut tx,
                "UPDATE source_repositories SET cache_hits=cache_hits+1 WHERE id=$1",
                params![id],
            )
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
        let mut protection_db = self.pool.clone();
        let mut protection = protection_db.transaction().await?;
        sql::execute(
            &mut protection,
            "SELECT pg_advisory_xact_lock_shared(hashtextextended($1,1))",
            params![project.to_string()],
        )
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
        Ok(sql::scalar(&mut self.pool.clone(), "INSERT INTO source_snapshots(id,project_id,root_repository_id,commit_sha,manifest,digest) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(project_id,digest) DO UPDATE SET last_used_at=now() RETURNING manifest", params![id, project, root, commit, manifest, digest]).await?)
    }
    pub async fn restore_repository<T, F, Fut>(&self, id: Uuid, restore: F) -> Result<T>
    where
        F: FnOnce(RepositoryAccess) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let row = models::Repository::get_by_id(&mut self.pool.clone(), id).await?;
        let access = RepositoryAccess {
            repository_url: row.repository_url,
            credential_id: row.credential_id,
            cache_key: row.cache_key,
        };
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        sql::execute(
            &mut tx,
            "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
            params![&access.cache_key],
        )
        .await?;
        let result = restore(access).await?;
        tx.commit().await?;
        Ok(result)
    }
    pub async fn touch_snapshot(&self, id: Uuid) -> Result<()> {
        sql::execute(
            &mut self.pool.clone(),
            "UPDATE source_snapshots SET last_used_at=now() WHERE id=$1",
            params![id],
        )
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
            let mut tx_db = store.pool.clone();
            let protected = async {
                let key = store.cache_key(repository).await?;
                let mut tx = tx_db.transaction().await?;
                sql::execute(
                    &mut tx,
                    "SELECT pg_advisory_xact_lock_shared(hashtextextended($1,0))",
                    params![key],
                )
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
        sql::execute(&mut self.pool.clone(), "UPDATE source_repositories SET git_download_bytes=git_download_bytes+CASE WHEN $3 THEN 0 ELSE $2 END,lfs_download_bytes=lfs_download_bytes+CASE WHEN $3 THEN $2 ELSE 0 END,last_used_at=now() WHERE id=$1", params![id, bytes, lfs]).await?;
        Ok(())
    }
}

impl SourceStore {
    pub async fn pinned_root(&self, operation: Uuid) -> Result<Option<String>> {
        Ok(models::Operation::filter_by_id(operation)
            .select(models::Operation::fields().root_commit())
            .one()
            .exec(&mut self.pool.clone())
            .await?)
    }
    pub async fn pin_root(&self, operation: Uuid, commit: &str) -> Result<()> {
        sql::execute(
            &mut self.pool.clone(),
            "UPDATE source_operations SET root_commit=coalesce(root_commit,$2) WHERE id=$1",
            params![operation, commit],
        )
        .await?;
        Ok(())
    }
    pub async fn frozen_tags(&self, operation: Uuid, path: &str) -> Result<Option<Value>> {
        Ok(
            models::OperationRepository::filter_by_operation_id_and_path(operation, path)
                .select(models::OperationRepository::fields().tags())
                .first()
                .exec(&mut self.pool.clone())
                .await?
                .map(|tags| tags.0),
        )
    }
    pub async fn freeze_tags(&self, operation: Uuid, path: &str, tags: Value) -> Result<()> {
        models::OperationRepository::upsert_by_operation_id_and_path(operation, path)
            .tags(toasty::Json(tags))
            .or_ignore()
            .exec(&mut self.pool.clone())
            .await?;
        Ok(())
    }
}

mod service;
pub use service::SourceMetrics;

pub fn shared_models() -> toasty::schema::app::ModelSet {
    toasty::models!(
        models::Repository,
        models::Snapshot,
        models::Operation,
        models::OperationRepository,
        models::Retention
    )
}
pub fn models() -> toasty::schema::app::ModelSet {
    let mut models = shared_models();
    for model in toasty::models!(models::ServiceCredential) {
        models.add(model);
    }
    models
}
pub fn embedded_models() -> toasty::schema::app::ModelSet {
    let mut models = shared_models();
    for model in toasty::models!(models::ControlCredential) {
        models.add(model);
    }
    models
}
