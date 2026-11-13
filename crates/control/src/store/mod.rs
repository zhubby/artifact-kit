//! Persistence operations for the control plane.

mod auth;
mod management;
mod planning_management;
mod projects;

use sqlx::PgPool;

pub use auth::LoginAccount;
pub use projects::{ProjectInput, ProjectSource};

/// Shared control-plane persistence; transport code calls business operations.
#[derive(Clone)]
pub struct ControlStore {
    pool: PgPool,
    pub source: artifactory_source_core::store::SourceStore,
}

impl ControlStore {
    pub fn new(pool: PgPool) -> Self {
        Self {
            source: artifactory_source_core::store::SourceStore::new(pool.clone()),
            pool,
        }
    }

    pub async fn audit(
        &self,
        actor: String,
        action: &str,
        resource: String,
        details: serde_json::Value,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO audit_events(actor,action,resource_id,details) VALUES($1,$2,$3,$4)",
        )
        .bind(actor)
        .bind(action)
        .bind(resource)
        .bind(details)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

pub use management::{NewToken, NodeUpdate};
pub use planning_management::{ProfileInput, WorkerUpdate};

mod ota_management;
pub use ota_management::Target;

mod builds;
pub use builds::{ArtifactDownload, BuildInput, LogChunk};

mod scheduling;
pub(crate) use scheduling::ResolvedSource;

mod executions;
pub(crate) use executions::{
    ArtifactDeclaration, EventReceipt, ExecutionEvent, LogCommit, NodeEnrollment,
};

mod lookups;
mod ota;

mod planning;
pub(crate) use planning::{
    PlanningAction, PlanningPrepared, PlanningToolResult, SubmissionContext,
};

mod runtime_bridge;
pub(crate) use runtime_bridge::{RemoteSnapshot, RuntimeEventCommit};

mod source_cache;

impl ControlStore {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(20)
            .connect(url)
            .await?;
        artifactory_database::require_timescale(&pool).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self::new(pool))
    }
    pub async fn users_empty(&self) -> anyhow::Result<bool> {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
            .fetch_one(&self.pool)
            .await?;
        Ok(count == 0)
    }
    pub async fn bootstrap_admin(&self, username: String, hash: String) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO users(id,username,password_hash,role) VALUES($1,$2,$3,'admin') ON CONFLICT(username) DO NOTHING").bind(uuid::Uuid::new_v4()).bind(username).bind(hash).execute(&self.pool).await?;
        Ok(())
    }
}

mod errors;

#[cfg(test)]
mod performance;
