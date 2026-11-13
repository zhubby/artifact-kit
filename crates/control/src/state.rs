use crate::{protocol::Command, settings::Settings, storage::Storage};
use sqlx::PgPool;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

#[derive(Clone)]
pub struct Connection {
    pub session: Uuid,
    pub sender: mpsc::Sender<std::result::Result<Command, tonic::Status>>,
}

#[derive(Clone)]
pub struct AppState {
    pub coordination: Arc<crate::coordination::Coordination>,
    pub source_client: Arc<tokio::sync::OnceCell<artifactory_runtime_protocol::service::Client>>,
    pub pool: PgPool,
    pub settings: Arc<Settings>,
    pub storage: Arc<Storage>,
    pub connections: Arc<RwLock<HashMap<Uuid, Connection>>>,
}

impl AppState {
    pub async fn audit(
        &self,
        actor: impl ToString,
        action: &str,
        resource: impl ToString,
        details: serde_json::Value,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO audit_events(actor,action,resource_id,details) VALUES($1,$2,$3,$4)",
        )
        .bind(actor.to_string())
        .bind(action)
        .bind(resource.to_string())
        .bind(details)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
