pub use artifactory_runtime_protocol::crypto;
mod cache;
pub mod source;
pub mod store;
use anyhow::{Result, ensure};
pub use cache::*;
use std::{path::PathBuf, sync::Arc};
#[derive(Clone)]
pub struct Settings {
    pub data_dir: PathBuf,
    pub master_key: [u8; 32],
}
#[derive(Clone)]
pub struct AppState {
    pub store: store::SourceStore,
    pub settings: Arc<Settings>,
}
pub fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= 2048
            && !path.starts_with('/')
            && !path.contains(['\\', '\0', '\n', '\r', ':'])
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "invalid source path"
    );
    Ok(())
}
