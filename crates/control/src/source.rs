use crate::{crypto, state::AppState};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

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
        let encrypted: String =
            sqlx::query_scalar("SELECT ciphertext FROM credentials WHERE id=$1")
                .bind(id)
                .fetch_one(&state.pool)
                .await?;
        Ok(Some(serde_json::from_slice(&crypto::open(
            &state.settings.master_key,
            &encrypted,
        )?)?))
    } else {
        Ok(None)
    }
}

pub async fn resolve(
    state: &AppState,
    repository: &str,
    reference: &str,
    credential: Option<GitCredential>,
) -> Result<(String, String)> {
    validate_repository(repository)?;
    ensure!(
        !reference.is_empty()
            && reference.len() <= 255
            && !reference.starts_with('-')
            && !reference.contains(['\0', '\n', '\r']),
        "invalid Git reference"
    );
    let directory = tempfile::tempdir_in(&state.settings.data_dir)?;
    let repo = directory.path().join("source.git");
    let mut environment = vec![
        ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
    ];
    if let Some(auth) = credential {
        if auth.kind == "ssh" {
            ensure!(
                !auth.known_hosts.trim().is_empty(),
                "SSH host fingerprints are required"
            );
            let key = directory.path().join("identity");
            let hosts = directory.path().join("known_hosts");
            tokio::fs::write(&key, auth.secret).await?;
            tokio::fs::write(&hosts, auth.known_hosts).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).await?;
            }
            environment.push(("GIT_SSH_COMMAND".into(), format!("ssh -F /dev/null -i '{}' -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile='{}'", key.display(), hosts.display())));
        } else {
            let askpass = directory.path().join("askpass.sh");
            tokio::fs::write(&askpass, "#!/bin/sh\ncase \"$1\" in *sername*) printf '%s\\n' \"$AK_GIT_USERNAME\";; *) printf '%s\\n' \"$AK_GIT_PASSWORD\";; esac\n").await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(&askpass, std::fs::Permissions::from_mode(0o700))
                    .await?;
            }
            environment.extend([
                ("GIT_ASKPASS".into(), askpass.to_string_lossy().into()),
                ("AK_GIT_USERNAME".into(), auth.username),
                ("AK_GIT_PASSWORD".into(), auth.secret),
            ]);
        }
    }
    run_git(
        &[
            "init",
            "--bare",
            repo.to_str().context("invalid managed path")?,
        ],
        &environment,
    )
    .await?;
    let repo_path = repo.to_str().unwrap();
    run_git(
        &[
            "-C",
            repo_path,
            "fetch",
            "--depth=1",
            "--no-tags",
            "--",
            repository,
            reference,
        ],
        &environment,
    )
    .await?;
    let commit = run_git(
        &[
            "-C",
            repo_path,
            "rev-parse",
            "--verify",
            "FETCH_HEAD^{commit}",
        ],
        &environment,
    )
    .await?;
    let commit = commit.trim().to_owned();
    ensure!(
        commit.len() == 40 || commit.len() == 64,
        "invalid resolved commit"
    );
    let object = format!("{commit}:artifact-kit.yaml");
    let size = run_git(&["-C", repo_path, "cat-file", "-s", &object], &environment).await?;
    ensure!(
        size.trim().parse::<usize>()? <= 256 * 1024,
        "configuration exceeds 256 KiB"
    );
    let config = run_git(&["-C", repo_path, "show", &object], &environment).await?;
    Ok((commit, config))
}

async fn run_git(arguments: &[&str], environment: &[(String, String)]) -> Result<String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        Command::new("git")
            .args(arguments)
            .envs(environment.iter().cloned())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("Git operation timed out")??;
    ensure!(
        output.status.success(),
        "Git operation failed; check repository access, reference, host trust and root artifact-kit.yaml"
    );
    Ok(String::from_utf8(output.stdout)?)
}
