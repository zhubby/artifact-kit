use crate::{AppState, crypto};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitCredential {
    pub kind: String,
    #[serde(default)]
    pub username: String,
    pub secret: String,
    #[serde(default)]
    pub known_hosts: String,
}

pub fn validate_repository(repository: &str) -> Result<()> {
    ensure!(
        repository.len() <= 2048 && !repository.contains(['\0', '\n', '\r']),
        "invalid repository URL"
    );
    if repository.starts_with("https://") || repository.starts_with("ssh://") {
        let url = url::Url::parse(repository)?;
        ensure!(
            url.host_str().is_some()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "invalid repository URL"
        );
        ensure!(
            url.scheme() != "https" || url.username().is_empty(),
            "use a credential reference, not URL credentials"
        );
    } else {
        let (host, path) = repository
            .split_once(':')
            .context("only SSH and HTTPS Git repositories are supported")?;
        ensure!(
            host.contains('@')
                && !host.starts_with('-')
                && host
                    .bytes()
                    .all(|value| value.is_ascii_alphanumeric() || b"@._-".contains(&value)),
            "invalid SSH repository"
        );
        ensure!(
            !path.is_empty() && !path.starts_with('-'),
            "invalid SSH path"
        );
    }
    Ok(())
}

pub async fn credential(state: &AppState, id: Option<uuid::Uuid>) -> Result<Option<GitCredential>> {
    if let Some(id) = id {
        let encrypted = state.store.credential_ciphertext(id).await?;
        Ok(Some(serde_json::from_slice(&crypto::open(
            &state.settings.master_key,
            &encrypted,
        )?)?))
    } else {
        Ok(None)
    }
}
