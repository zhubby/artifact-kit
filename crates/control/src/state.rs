use crate::{protocol::Command, settings::Settings, storage::Storage};
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
    pub store: crate::store::ControlStore,
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
        self.store
            .audit(actor.to_string(), action, resource.to_string(), details)
            .await?;
        Ok(())
    }
}
