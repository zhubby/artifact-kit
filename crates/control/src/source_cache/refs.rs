use super::{authentication, valid_commit};
use crate::{source, state::AppState};
use serde::Serialize;
use std::{collections::BTreeMap, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct GitRef {
    pub name: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub oid: String,
}

#[derive(Debug, Serialize)]
pub struct RepositoryRefs {
    pub default_ref: Option<String>,
    pub branches: Vec<GitRef>,
    pub tags: Vec<GitRef>,
}

#[derive(Debug, thiserror::Error)]
pub enum RefQueryError {
    #[error(
        "Unable to read repository references; check repository access, credentials and host trust."
    )]
    Unavailable,
    #[error("Repository reference query timed out; try again.")]
    Timeout,
    #[error("Repository reference list exceeds the 4 MiB limit; enter a reference manually.")]
    TooLarge,
    #[error(
        "Repository returned an invalid reference list; try again or enter a reference manually."
    )]
    Invalid,
}

pub async fn list_refs(
    state: &AppState,
    repository: &str,
    credential_id: Option<Uuid>,
) -> Result<RepositoryRefs, RefQueryError> {
    source::validate_repository(repository).map_err(|_| RefQueryError::Unavailable)?;
    let auth = authentication(state, credential_id)
        .await
        .map_err(|_| RefQueryError::Unavailable)?;
    let mut command = Command::new("git");
    command
        .current_dir(auth._directory.path())
        .env("HOME", auth._directory.path())
        .env("XDG_CONFIG_HOME", auth._directory.path())
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env_remove("SSH_AUTH_SOCK")
        .env_remove("GIT_SSH_VARIANT")
        .env("GIT_SSH_COMMAND", "ssh -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityFile=none -o IdentityAgent=none -o StrictHostKeyChecking=yes -o UserKnownHostsFile=/dev/null -o GlobalKnownHostsFile=/dev/null")
        .args([
            "-c",
            "credential.helper=",
            "-c",
            "core.askPass=",
            "-c",
            "core.hooksPath=/dev/null",
            "ls-remote",
            "--symref",
            "--",
            repository,
            "HEAD",
            "refs/heads/*",
            "refs/tags/*",
        ])
        .envs(auth.environment.iter().cloned());
    let output = query_output(command, Duration::from_secs(30), 4 * 1024 * 1024).await?;
    parse_refs(&output)
}

async fn query_output(
    mut command: Command,
    timeout: Duration,
    limit: usize,
) -> Result<Vec<u8>, RefQueryError> {
    let mut child = command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| RefQueryError::Unavailable)?;
    let stdout = child.stdout.take().ok_or(RefQueryError::Unavailable)?;
    tokio::time::timeout(timeout, async {
        let mut output = Vec::new();
        stdout
            .take(limit as u64 + 1)
            .read_to_end(&mut output)
            .await
            .map_err(|_| RefQueryError::Unavailable)?;
        if output.len() > limit {
            return Err(RefQueryError::TooLarge);
        }
        let status = child.wait().await.map_err(|_| RefQueryError::Unavailable)?;
        if !status.success() {
            return Err(RefQueryError::Unavailable);
        }
        Ok(output)
    })
    .await
    .map_err(|_| RefQueryError::Timeout)?
}

fn parse_refs(output: &[u8]) -> Result<RepositoryRefs, RefQueryError> {
    let text = std::str::from_utf8(output).map_err(|_| RefQueryError::Invalid)?;
    let mut default_ref = None;
    let mut branches = BTreeMap::new();
    let mut tags = BTreeMap::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let (value, reference) = line.split_once('\t').ok_or(RefQueryError::Invalid)?;
        if let Some(symbolic) = value.strip_prefix("ref: ") {
            if reference == "HEAD" && symbolic.starts_with("refs/heads/") {
                default_ref = Some(symbolic.to_owned());
            }
            continue;
        }
        if !valid_commit(value) {
            return Err(RefQueryError::Invalid);
        }
        let (entries, name) = if let Some(name) = reference.strip_prefix("refs/heads/") {
            (&mut branches, name)
        } else if let Some(name) = reference.strip_prefix("refs/tags/") {
            if name.ends_with("^{}") {
                continue;
            }
            (&mut tags, name)
        } else {
            continue;
        };
        if name.is_empty() || name.chars().any(char::is_control) {
            return Err(RefQueryError::Invalid);
        }
        entries.insert(
            reference.to_owned(),
            GitRef {
                name: name.to_owned(),
                reference: reference.to_owned(),
                oid: value.to_owned(),
            },
        );
    }
    Ok(RepositoryRefs {
        default_ref: default_ref.filter(|reference| branches.contains_key(reference)),
        branches: branches.into_values().collect(),
        tags: tags.into_values().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_branch_and_distinguishes_same_named_tags() {
        let branch_oid = "a".repeat(40);
        let tag_oid = "b".repeat(40);
        let output = format!(
            "ref: refs/heads/trunk\tHEAD\n{branch_oid}\tHEAD\n{branch_oid}\trefs/heads/trunk\n{branch_oid}\trefs/heads/v1\n{tag_oid}\trefs/tags/v1\n{branch_oid}\trefs/tags/v1^{{}}\n{branch_oid}\trefs/remotes/origin/HEAD\n"
        );
        let result = parse_refs(output.as_bytes()).unwrap();
        assert_eq!(result.default_ref.as_deref(), Some("refs/heads/trunk"));
        assert_eq!(result.branches.len(), 2);
        assert_eq!(result.tags.len(), 1);
        assert_eq!(result.tags[0].name, "v1");
        assert_eq!(result.tags[0].reference, "refs/tags/v1");
        assert_eq!(result.tags[0].oid, tag_oid);
        assert_eq!(result.branches[1].reference, "refs/heads/v1");
    }

    #[test]
    fn missing_or_unborn_default_branch_is_not_guessed() {
        let oid = "a".repeat(40);
        for output in [
            format!("{oid}\tHEAD\n{oid}\trefs/heads/main\n"),
            "ref: refs/heads/unborn\tHEAD\n".into(),
            String::new(),
        ] {
            assert!(parse_refs(output.as_bytes()).unwrap().default_ref.is_none());
        }
        let empty = parse_refs(b"").unwrap();
        assert!(empty.branches.is_empty());
        assert!(empty.tags.is_empty());
    }

    #[test]
    fn supports_sha256_and_deduplicates_and_sorts_references() {
        let oid = "c".repeat(64);
        let output = format!(
            "{oid}\trefs/heads/z-last\n{oid}\trefs/heads/a-first\n{oid}\trefs/heads/a-first\n"
        );
        let result = parse_refs(output.as_bytes()).unwrap();
        assert_eq!(result.branches.len(), 2);
        assert_eq!(result.branches[0].name, "a-first");
        assert_eq!(result.branches[0].oid, oid);
    }

    #[test]
    fn rejects_invalid_advertisements_without_returning_partial_results() {
        for output in [
            b"invalid\trefs/heads/main\n".as_slice(),
            b"unexpected output\n".as_slice(),
            b"\xff\n".as_slice(),
        ] {
            assert!(parse_refs(output).is_err());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn query_enforces_output_limit() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "printf '12345'"]);
        assert!(matches!(
            query_output(command, std::time::Duration::from_secs(1), 4).await,
            Err(RefQueryError::TooLarge)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn query_times_out_without_waiting_for_git() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "exec sleep 10"]);
        assert!(matches!(
            query_output(command, std::time::Duration::from_millis(20), 1024).await,
            Err(RefQueryError::Timeout)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn query_errors_never_expose_git_stderr() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "printf 'private-key-material' >&2; exit 1"]);
        let error = query_output(command, std::time::Duration::from_secs(1), 1024)
            .await
            .unwrap_err();
        assert!(matches!(error, RefQueryError::Unavailable));
        assert!(!error.to_string().contains("private-key-material"));
    }
}
