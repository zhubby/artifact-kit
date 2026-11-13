//! Immutable source snapshots backed by project-scoped Git/LFS object caches.
use crate::{crypto, execution_plan::validate_path, source, state::AppState};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use uuid::Uuid;

mod refs;
pub use refs::{GitRef, RefQueryError, RepositoryRefs, list_refs};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LfsObject {
    pub path: String,
    pub oid: String,
    pub size: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Repository {
    pub id: Uuid,
    pub repository_id: Uuid,
    pub path: String,
    pub commit: String,
    pub tags: BTreeMap<String, String>,
    pub lfs: Vec<LfsObject>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Snapshot {
    pub id: Uuid,
    pub project_id: Uuid,
    pub commit: String,
    pub repositories: Vec<Repository>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialBinding {
    pub repository_url: String,
    pub credential_id: Uuid,
}

pub fn valid_commit(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64)
        && commit
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}
pub fn valid_oid(oid: &str) -> bool {
    oid.len() == 64 && valid_commit(oid)
}

pub async fn load(state: &AppState, id: Uuid) -> Result<Snapshot> {
    let value: Value = sqlx::query_scalar("SELECT manifest FROM source_snapshots WHERE id=$1")
        .bind(id)
        .fetch_one(&state.pool)
        .await?;
    Ok(serde_json::from_value(value)?)
}

pub async fn repository_path(state: &AppState, id: Uuid) -> Result<PathBuf> {
    let key: String = sqlx::query_scalar("SELECT cache_key FROM source_repositories WHERE id=$1")
        .bind(id)
        .fetch_one(&state.pool)
        .await?;
    ensure!(valid_oid(&key), "invalid source cache key");
    Ok(state
        .settings
        .data_dir
        .join("sources/repos")
        .join(format!("{key}.git")))
}

fn origin(repository: &str) -> Option<String> {
    let url = if repository.starts_with("https://") || repository.starts_with("ssh://") {
        url::Url::parse(repository).ok()?
    } else {
        let (host, path) = repository.split_once(':')?;
        url::Url::parse(&format!("ssh://{host}/{path}")).ok()?
    };
    Some(format!(
        "{}://{}:{}",
        url.scheme(),
        url.host_str()?,
        url.port_or_known_default().unwrap_or(22)
    ))
}

pub fn resolve_submodule(parent: &str, child: &str) -> Result<String> {
    ensure!(
        !child.contains(['\0', '\n', '\r']) && child.len() <= 2048,
        "invalid submodule URL"
    );
    let result = if child.starts_with("./") || child.starts_with("../") {
        let base = if parent.contains("://") {
            parent.to_string()
        } else {
            let (host, path) = parent
                .split_once(':')
                .context("invalid parent repository")?;
            format!("ssh://{host}/{path}")
        };
        url::Url::parse(&format!("{}/", base.trim_end_matches('/')))?
            .join(child)?
            .to_string()
    } else {
        child.into()
    };
    source::validate_repository(&result)?;
    Ok(result)
}

struct Authentication {
    _directory: tempfile::TempDir,
    environment: Vec<(String, String)>,
}
async fn authentication(state: &AppState, credential_id: Option<Uuid>) -> Result<Authentication> {
    let directory = tempfile::tempdir_in(&state.settings.data_dir)?;
    let mut environment = vec![
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_LFS_SKIP_SMUDGE".into(), "1".into()),
    ];
    if let Some(auth) = source::credential(state, credential_id).await? {
        if auth.kind == "ssh" {
            ensure!(
                !auth.known_hosts.trim().is_empty(),
                "SSH host trust is required"
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
            let trusted = format!(
                "UserKnownHostsFile=\"{}\"",
                hosts
                    .to_string_lossy()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('%', "%%")
            );
            environment.push(("GIT_SSH_COMMAND".into(),format!("ssh -F /dev/null -i {} -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=yes -o {}",quote(&key.to_string_lossy()),quote(&trusted))));
        } else {
            ensure!(auth.kind == "https", "unsupported Git credential");
            let path = directory.path().join("askpass.sh");
            tokio::fs::write(&path,"#!/bin/sh\ncase \"$1\" in *sername*) printf '%s\\n' \"$ARTIFACTORY_GIT_USERNAME\";; *) printf '%s\\n' \"$ARTIFACTORY_GIT_PASSWORD\";; esac\n").await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).await?;
            }
            environment.extend([
                ("GIT_ASKPASS".into(), path.to_string_lossy().into()),
                ("ARTIFACTORY_GIT_USERNAME".into(), auth.username),
                ("ARTIFACTORY_GIT_PASSWORD".into(), auth.secret),
            ]);
        }
    }
    Ok(Authentication {
        _directory: directory,
        environment,
    })
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub async fn git(repo: &Path, arguments: &[&str]) -> Result<Vec<u8>> {
    git_with(repo, arguments, &[]).await
}
async fn git_with(
    repo: &Path,
    arguments: &[&str],
    environment: &[(String, String)],
) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("credential.helper=")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-C")
        .arg(repo)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .envs(environment.iter().cloned())
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn()?;
    let mut stdout = child.stdout.take().context("Git stdout unavailable")?;
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(300), async {
        (&mut stdout)
            .take(32 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            bytes.len() <= 32 * 1024 * 1024,
            "Git response exceeds limit"
        );
        let status = child.wait().await?;
        ensure!(
            status.success(),
            "Git operation failed; check source access, commit and host trust"
        );
        Ok::<_, anyhow::Error>(bytes)
    })
    .await
    .context("Git operation timed out")?
}

async fn disk_size(path: PathBuf) -> Result<u64> {
    tokio::task::spawn_blocking(move || {
        fn visit(path: &Path) -> std::io::Result<u64> {
            if !path.exists() {
                return Ok(0);
            }
            let metadata = std::fs::symlink_metadata(path)?;
            if !metadata.is_dir() {
                return Ok(metadata.len());
            }
            std::fs::read_dir(path)?.try_fold(0, |sum, entry| Ok(sum + visit(&entry?.path())?))
        }
        visit(&path)
    })
    .await?
    .map_err(Into::into)
}

async fn fetch(
    state: &AppState,
    project: Uuid,
    url: &str,
    reference: &str,
    auth_id: Option<Uuid>,
) -> Result<(Uuid, PathBuf, String)> {
    source::validate_repository(url)?;
    ensure!(
        !reference.is_empty()
            && reference.len() <= 255
            && !reference.starts_with('-')
            && !reference.contains(['\0', '\n', '\r']),
        "invalid Git reference"
    );
    let credential_revision: String = if let Some(id) = auth_id {
        sqlx::query_scalar("SELECT ciphertext FROM credentials WHERE id=$1")
            .bind(id)
            .fetch_one(&state.pool)
            .await?
    } else {
        String::new()
    };
    let key = crypto::hash(format!(
        "{project}\n{url}\n{}",
        crypto::hash(credential_revision)
    ));
    let request_started = Utc::now();
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(&key)
        .execute(&mut *tx)
        .await?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO source_repositories(id,project_id,repository_url,credential_id,cache_key) VALUES($1,$2,$3,$4,$5) ON CONFLICT(cache_key) DO UPDATE SET last_used_at=now() RETURNING id")
        .bind(Uuid::new_v4()).bind(project).bind(url).bind(auth_id).bind(&key).fetch_one(&mut *tx).await?;
    let path = state
        .settings
        .data_dir
        .join("sources/repos")
        .join(format!("{key}.git"));
    tokio::fs::create_dir_all(path.parent().unwrap()).await?;
    let auth = authentication(state, auth_id).await?;
    if !path.join("HEAD").exists() {
        tokio::fs::create_dir_all(&path).await?;
        git_with(&path, &["init", "--bare"], &auth.environment).await?;
        git(&path, &["config", "http.receivepack", "false"]).await?;
        git(&path, &["config", "gc.auto", "0"]).await?;
        git(&path, &["config", "remote.origin.url", url]).await?;
    }
    let coalesced: Option<String> = sqlx::query_scalar("SELECT commit_sha FROM source_resolutions WHERE repository_id=$1 AND reference=$2 AND fetched_at>=$3")
        .bind(id).bind(reference).bind(request_started).fetch_optional(&mut *tx).await?;
    let existing = valid_commit(reference)
        && git(
            &path,
            &["cat-file", "-e", &format!("{reference}^{{commit}}")],
        )
        .await
        .is_ok();
    let coalesced = if let Some(commit) = coalesced {
        if git(&path, &["cat-file", "-e", &format!("{commit}^{{commit}}")])
            .await
            .is_ok()
        {
            Some(commit)
        } else {
            None
        }
    } else {
        None
    };
    let commit = if existing {
        reference.to_owned()
    } else if let Some(commit) = coalesced {
        commit
    } else {
        let before = disk_size(path.clone()).await?;
        git_with(
            &path,
            &[
                "fetch",
                "--force",
                "--tags",
                "--no-recurse-submodules",
                "--",
                url,
                reference,
            ],
            &auth.environment,
        )
        .await?;
        let commit = String::from_utf8(
            git(&path, &["rev-parse", "--verify", "FETCH_HEAD^{commit}"]).await?,
        )?
        .trim()
        .to_owned();
        ensure!(valid_commit(&commit), "invalid resolved commit");
        git(
            &path,
            &[
                "update-ref",
                &format!("refs/artifactory/commits/{commit}"),
                &commit,
            ],
        )
        .await?;
        let delta = disk_size(path.clone()).await?.saturating_sub(before);
        sqlx::query("UPDATE source_repositories SET fetched_bytes=fetched_bytes+$2,cache_hits=cache_hits+CASE WHEN $2=0 THEN 1 ELSE 0 END WHERE id=$1").bind(id).bind(delta as i64).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO source_resolutions(repository_id,reference,commit_sha) VALUES($1,$2,$3) ON CONFLICT(repository_id,reference) DO UPDATE SET commit_sha=$3,fetched_at=clock_timestamp()")
            .bind(id).bind(reference).bind(&commit).execute(&mut *tx).await?;
        commit
    };
    if existing {
        sqlx::query("UPDATE source_repositories SET cache_hits=cache_hits+1 WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok((id, path, commit))
}

async fn tree(repo: &Path, commit: &str) -> Result<Vec<(String, String, String, u64)>> {
    let data = git(repo, &["ls-tree", "-r", "-l", "-z", commit]).await?;
    let mut result = Vec::new();
    for row in data.split(|c| *c == 0).filter(|row| !row.is_empty()) {
        let row = std::str::from_utf8(row)?;
        let (metadata, path) = row.split_once('\t').context("invalid Git tree")?;
        validate_path(path)?;
        let fields: Vec<_> = metadata.split_whitespace().collect();
        ensure!(fields.len() == 4, "invalid Git tree entry");
        result.push((
            fields[0].into(),
            path.into(),
            fields[2].into(),
            fields[3].parse().unwrap_or(0),
        ));
    }
    Ok(result)
}

fn pointer(path: &str, content: &[u8]) -> Result<Option<LfsObject>> {
    if !content.starts_with(b"version https://git-lfs.github.com/spec/v1\n") {
        return Ok(None);
    }
    let text = std::str::from_utf8(content)?;
    let oid = text
        .lines()
        .find_map(|line| line.strip_prefix("oid sha256:"))
        .context("invalid LFS pointer")?;
    let size: u64 = text
        .lines()
        .find_map(|line| line.strip_prefix("size "))
        .context("invalid LFS size")?
        .parse()?;
    ensure!(
        valid_oid(oid) && size <= 20 * 1024 * 1024 * 1024,
        "invalid LFS object"
    );
    Ok(Some(LfsObject {
        path: path.into(),
        oid: oid.into(),
        size,
    }))
}

pub fn lfs_path(repo: &Path, oid: &str) -> Result<PathBuf> {
    ensure!(valid_oid(oid), "invalid LFS object ID");
    Ok(repo
        .join("lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(oid))
}
async fn verify_lfs(repo: &Path, item: &LfsObject) -> Result<()> {
    let path = lfs_path(repo, &item.oid)?;
    ensure!(
        tokio::fs::metadata(&path).await?.len() == item.size,
        "LFS object size mismatch"
    );
    let mut file = tokio::fs::File::open(path).await?;
    let mut hash = Sha256::new();
    let mut bytes = vec![0u8; 128 * 1024];
    loop {
        let count = file.read(&mut bytes).await?;
        if count == 0 {
            break;
        }
        hash.update(&bytes[..count]);
    }
    ensure!(
        hex::encode(hash.finalize()) == item.oid,
        "LFS object digest mismatch"
    );
    Ok(())
}

async fn inspect(
    state: &AppState,
    repo: &Path,
    commit: &str,
    auth_id: Option<Uuid>,
) -> Result<(
    BTreeMap<String, String>,
    Vec<LfsObject>,
    Vec<(String, String, String)>,
)> {
    let entries = tree(repo, commit).await?;
    let mut modules = BTreeMap::<String, BTreeMap<String, String>>::new();
    if git(repo, &["cat-file", "-e", &format!("{commit}:.gitmodules")])
        .await
        .is_ok()
    {
        let data = git(
            repo,
            &[
                "config",
                "--null",
                "--blob",
                &format!("{commit}:.gitmodules"),
                "--get-regexp",
                "^submodule\\..*\\.(path|url)$",
            ],
        )
        .await?;
        for item in data.split(|c| *c == 0).filter(|item| !item.is_empty()) {
            let item = std::str::from_utf8(item)?;
            let (key, value) = item
                .split_once('\n')
                .context("invalid submodule configuration")?;
            let (name, field) = key.rsplit_once('.').context("invalid submodule key")?;
            modules
                .entry(name.into())
                .or_default()
                .insert(field.into(), value.into());
        }
    }
    let by_path: BTreeMap<_, _> = modules
        .values()
        .map(|item| {
            Ok((
                item.get("path").context("missing submodule path")?.clone(),
                item.get("url").context("missing submodule URL")?.clone(),
            ))
        })
        .collect::<Result<_>>()?;
    let mut children = Vec::new();
    let mut lfs = Vec::new();
    // Read candidate pointer blobs in one process; small ordinary files stay cheap.
    let candidates: Vec<_> = entries
        .iter()
        .filter(|(mode, _, _, size)| (mode == "100644" || mode == "100755") && *size <= 1024)
        .collect();
    if !candidates.is_empty() {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(repo)
            .args(["cat-file", "--batch"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        let input = candidates
            .iter()
            .map(|(_, _, oid, _)| format!("{oid}\n"))
            .collect::<String>();
        let writer = tokio::spawn(async move { stdin.write_all(input.as_bytes()).await });
        let output = child.wait_with_output().await?;
        writer.await??;
        ensure!(output.status.success(), "cannot inspect pointer blobs");
        let mut data = output.stdout.as_slice();
        for (_, path, _, _) in candidates {
            let newline = data
                .iter()
                .position(|c| *c == b'\n')
                .context("invalid blob output")?;
            let header = std::str::from_utf8(&data[..newline])?;
            let size: usize = header
                .split_whitespace()
                .last()
                .context("invalid blob size")?
                .parse()?;
            data = &data[newline + 1..];
            ensure!(data.len() > size, "incomplete blob output");
            if let Some(object) = pointer(path, &data[..size])? {
                lfs.push(object);
            }
            data = &data[size + 1..];
        }
    }
    for (mode, path, commit, _) in &entries {
        if mode == "160000" {
            children.push((
                path.clone(),
                by_path
                    .get(path)
                    .context("Git link has no submodule URL")?
                    .clone(),
                commit.clone(),
            ));
        }
    }
    ensure!(children.len() <= 128, "too many submodules");
    if !lfs.is_empty() {
        let auth = authentication(state, auth_id).await?;
        if lfs
            .iter()
            .any(|item| !lfs_path(repo, &item.oid).is_ok_and(|p| p.exists()))
        {
            git_with(repo, &["lfs", "fetch", "origin", commit], &auth.environment)
                .await
                .context("Git LFS fetch failed; install git-lfs and check object credentials")?;
        }
        for item in &lfs {
            verify_lfs(repo, item).await?;
        }
    }
    let raw = String::from_utf8(
        git(
            repo,
            &[
                "for-each-ref",
                "--format=%(refname:strip=2) %(objectname)",
                "refs/tags",
            ],
        )
        .await?,
    )?;
    let mut tags = BTreeMap::new();
    for line in raw.lines() {
        if let Some((name, oid)) = line.split_once(' ') {
            let ancestor = format!("{oid}^{{commit}}");
            if git(repo, &["merge-base", "--is-ancestor", &ancestor, commit])
                .await
                .is_ok()
            {
                tags.insert(name.into(), oid.into());
            }
        }
    }
    Ok((tags, lfs, children))
}

pub async fn capture(
    state: &AppState,
    project: Uuid,
    url: &str,
    reference: &str,
    credential_id: Option<Uuid>,
    bindings: Vec<CredentialBinding>,
) -> Result<Snapshot> {
    let mut protection = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtextextended($1,1))")
        .bind(project.to_string())
        .execute(&mut *protection)
        .await?;
    let mut pending = vec![(
        String::new(),
        url.to_string(),
        reference.to_string(),
        credential_id,
        0u32,
    )];
    let mut repositories = Vec::new();
    while let Some((path, url, reference, auth_id, depth)) = pending.pop() {
        ensure!(
            depth <= 16 && repositories.len() < 256,
            "source submodule graph exceeds limit"
        );
        let (repository_id, repo, commit) =
            fetch(state, project, &url, &reference, auth_id).await?;
        let (tags, lfs, children) = inspect(state, &repo, &commit, auth_id).await?;
        for (child_path, child_url, child_commit) in children.into_iter().rev() {
            let child_url = resolve_submodule(&url, &child_url)?;
            let child_auth = bindings
                .iter()
                .find(|binding| binding.repository_url == child_url)
                .map(|b| b.credential_id)
                .or_else(|| {
                    if origin(&url) == origin(&child_url) {
                        auth_id
                    } else {
                        None
                    }
                });
            let child_path = if path.is_empty() {
                child_path
            } else {
                format!("{path}/{child_path}")
            };
            validate_path(&child_path)?;
            pending.push((child_path, child_url, child_commit, child_auth, depth + 1));
        }
        repositories.push(Repository {
            id: Uuid::new_v4(),
            repository_id,
            path,
            commit,
            tags,
            lfs,
        });
    }
    repositories.sort_by(|a, b| a.path.cmp(&b.path));
    let root = repositories.first().context("empty source snapshot")?;
    ensure!(root.path.is_empty(), "missing root repository");
    let mut stable = serde_json::to_value(&repositories)?;
    for entry in stable.as_array_mut().unwrap() {
        entry.as_object_mut().unwrap().remove("id");
    }
    let digest = crypto::hash(serde_json::to_vec(&stable)?);
    let snapshot = Snapshot {
        id: Uuid::new_v4(),
        project_id: project,
        commit: root.commit.clone(),
        repositories,
    };
    let manifest: Value = sqlx::query_scalar("INSERT INTO source_snapshots(id,project_id,root_repository_id,commit_sha,manifest,digest) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(project_id,digest) DO UPDATE SET last_used_at=now() RETURNING manifest")
        .bind(snapshot.id).bind(project).bind(snapshot.repositories[0].repository_id).bind(&snapshot.commit).bind(serde_json::to_value(&snapshot)?).bind(digest).fetch_one(&state.pool).await?;
    let snapshot: Snapshot = serde_json::from_value(manifest)?;
    ensure_available(state, &snapshot).await?;
    protection.commit().await?;
    Ok(snapshot)
}

async fn recover_object(repo: &Path, url: &str, oid: &str, auth: &Authentication) -> Result<()> {
    if git_with(
        repo,
        &["fetch", "--no-recurse-submodules", "--", url, oid],
        &auth.environment,
    )
    .await
    .is_err()
    {
        // Ordinary upload-pack servers reject unadvertised object IDs, even
        // when those objects remain reachable through current refs. Fetch that
        // history into separate refs, then verify the original identity. Never
        // substitute a current branch or tag for the immutable snapshot.
        git_with(
            repo,
            &[
                "fetch",
                "--force",
                "--no-tags",
                "--no-recurse-submodules",
                "--",
                url,
                "+refs/heads/*:refs/artifactory/recovery/heads/*",
                "+refs/tags/*:refs/artifactory/recovery/tags/*",
            ],
            &auth.environment,
        )
        .await?;
    }
    git(repo, &["cat-file", "-e", oid]).await?;
    Ok(())
}

pub async fn ensure_available(state: &AppState, snapshot: &Snapshot) -> Result<()> {
    if let Some(origin) = crate::runtime_bridge::source_origin(state, snapshot.id).await? {
        let _: Value = crate::runtime_bridge::source(origin)?
            .post("/v1/metadata", &json!({"snapshot_id":snapshot.id}))
            .await?;
        return Ok(());
    }
    if let Some(client) = crate::local_source::client(state).await? {
        let _: Value = client
            .post(
                "/internal/local-source",
                &json!({"operation":"ensure","snapshot_id":snapshot.id}),
            )
            .await?;
        return Ok(());
    }
    for entry in &snapshot.repositories {
        let row = sqlx::query(
            "SELECT repository_url,credential_id,cache_key FROM source_repositories WHERE id=$1",
        )
        .bind(entry.repository_id)
        .fetch_one(&state.pool)
        .await?;
        let mut tx = state.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(row.get::<String, _>("cache_key"))
            .execute(&mut *tx)
            .await?;
        let repo = repository_path(state, entry.repository_id).await?;
        if !repo.join("HEAD").exists()
            || git(
                &repo,
                &["cat-file", "-e", &format!("{}^{{commit}}", entry.commit)],
            )
            .await
            .is_err()
        {
            let auth = authentication(state, row.get("credential_id")).await?;
            tokio::fs::create_dir_all(&repo).await?;
            git(&repo, &["init", "--bare"]).await?;
            git(&repo, &["config", "http.receivepack", "false"]).await?;
            git(
                &repo,
                &[
                    "config",
                    "remote.origin.url",
                    &row.get::<String, _>("repository_url"),
                ],
            )
            .await?;
            recover_object(
                &repo,
                &row.get::<String, _>("repository_url"),
                &entry.commit,
                &auth,
            )
            .await
            .context("pinned source is no longer available upstream")?;
        }
        // Partial cache loss can leave the commit present while its annotated
        // tag or LFS payload is missing. Restore those exact pinned identities.
        for oid in entry.tags.values() {
            if git(&repo, &["cat-file", "-e", oid]).await.is_err() {
                let auth = authentication(state, row.get("credential_id")).await?;
                recover_object(&repo, &row.get::<String, _>("repository_url"), oid, &auth)
                    .await
                    .context("pinned tag object is no longer available upstream")?;
            }
        }
        if entry
            .lfs
            .iter()
            .any(|item| !lfs_path(&repo, &item.oid).is_ok_and(|path| path.exists()))
        {
            let auth = authentication(state, row.get("credential_id")).await?;
            git_with(
                &repo,
                &["lfs", "fetch", "origin", &entry.commit],
                &auth.environment,
            )
            .await?;
        }
        let prefix = format!("refs/namespaces/{}/refs", namespace(snapshot.id, entry.id));
        git(
            &repo,
            &[
                "update-ref",
                &format!("{prefix}/heads/source"),
                &entry.commit,
            ],
        )
        .await?;
        for (name, oid) in &entry.tags {
            git(
                &repo,
                &["update-ref", &format!("{prefix}/tags/{name}"), oid],
            )
            .await?;
        }
        for item in &entry.lfs {
            verify_lfs(&repo, item).await?;
        }
        tx.commit().await?;
    }
    sqlx::query("UPDATE source_snapshots SET last_used_at=now() WHERE id=$1")
        .bind(snapshot.id)
        .execute(&state.pool)
        .await?;
    Ok(())
}

/// Whole-repository eviction preserves snapshot metadata. A retry can restore
/// only its pinned object IDs; it never resolves a moving branch again.
pub async fn collect(state: &AppState, quota: u64) -> Result<()> {
    let rows = sqlx::query(
        "SELECT id,project_id,cache_key FROM source_repositories ORDER BY last_used_at",
    )
    .fetch_all(&state.pool)
    .await?;
    let mut total = 0u64;
    let mut candidates = Vec::new();
    for row in rows {
        let id: Uuid = row.get("id");
        let path = repository_path(state, id).await?;
        let bytes = disk_size(path.clone()).await?;
        total = total.saturating_add(bytes);
        sqlx::query("UPDATE source_repositories SET cache_bytes=$2 WHERE id=$1")
            .bind(id)
            .bind(bytes as i64)
            .execute(&state.pool)
            .await?;
        candidates.push((row, path, bytes));
    }
    for (row, path, bytes) in candidates {
        if total <= quota {
            break;
        }
        if bytes == 0 {
            continue;
        }
        let id: Uuid = row.get("id");
        let mut tx = state.pool.begin().await?;
        let project: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1,1))")
                .bind(row.get::<Uuid, _>("project_id").to_string())
                .fetch_one(&mut *tx)
                .await?;
        if !project {
            continue;
        }
        let idle: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1,0))")
                .bind(row.get::<String, _>("cache_key"))
                .fetch_one(&mut *tx)
                .await?;
        if !idle {
            continue;
        }
        let protected:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM builds b JOIN source_snapshots s ON s.id=b.source_snapshot_id WHERE b.status NOT IN ('succeeded','failed','lost','canceled') AND s.manifest->'repositories' @> $1::jsonb)").bind(json!([{"repository_id":id}])).fetch_one(&mut *tx).await?;
        if protected {
            continue;
        }
        if path.exists() {
            tokio::fs::remove_dir_all(&path).await?;
        }
        sqlx::query("UPDATE source_repositories SET cache_bytes=0 WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        total = total.saturating_sub(bytes);
    }
    sqlx::query("DELETE FROM source_tokens WHERE expires_at<now()-interval '1 day'")
        .execute(&state.pool)
        .await?;
    Ok(())
}
pub fn namespace(snapshot: Uuid, entry: Uuid) -> String {
    format!("{}-{}", snapshot.simple(), entry.simple())
}

pub async fn descriptor(
    state: &AppState,
    snapshot_id: Uuid,
    attempt: Uuid,
) -> Result<crate::protocol::SourceDescriptor> {
    let snapshot = load(state, snapshot_id).await?;
    ensure_available(state, &snapshot).await?;
    let token = crypto::token();
    sqlx::query("INSERT INTO source_tokens(token_hash,snapshot_id,attempt_id,expires_at) VALUES($1,$2,$3,now()+interval '1 day')")
        .bind(crypto::hash(&token)).bind(snapshot_id).bind(attempt).execute(&state.pool).await?;
    let origin = crate::runtime_bridge::source_download_origin(state, snapshot_id)
        .await?
        .unwrap_or_else(|| state.settings.public_url.clone());
    let root = format!("{origin}/api/v1/source/{snapshot_id}");
    Ok(crate::protocol::SourceDescriptor {
        snapshot_id: snapshot_id.to_string(),
        access_token: token,
        repositories: snapshot
            .repositories
            .iter()
            .map(|r| crate::protocol::SourceRepository {
                path: r.path.clone(),
                repository_id: r.id.to_string(),
                commit_sha: r.commit.clone(),
                git_url: format!("{root}/{}/git", r.id),
                lfs_url: format!("{root}/{}/lfs", r.id),
                has_lfs: !r.lfs.is_empty(),
                tags: r.tags.clone().into_iter().collect(),
                cache_id: r.repository_id.to_string(),
            })
            .collect(),
    })
}

pub async fn validate_evidence(
    state: &AppState,
    snapshot: &Snapshot,
    paths: &[String],
) -> Result<()> {
    if let Some(origin) = crate::runtime_bridge::source_origin(state, snapshot.id).await? {
        let result: Value = crate::runtime_bridge::source(origin)?
            .post(
                "/v1/evidence",
                &json!({"snapshot_id":snapshot.id,"paths":paths}),
            )
            .await?;
        ensure!(
            result["valid"] == true,
            "evidence must reference an existing snapshot file"
        );
        return Ok(());
    }
    if let Some(client) = crate::local_source::client(state).await? {
        let _: Value = client
            .post(
                "/internal/local-source",
                &json!({"operation":"evidence","snapshot_id":snapshot.id,"paths":paths}),
            )
            .await?;
        return Ok(());
    }
    for path in paths {
        validate_path(path)?;
        let entry = snapshot
            .repositories
            .iter()
            .filter(|entry| entry.path.is_empty() || path.starts_with(&format!("{}/", entry.path)))
            .max_by_key(|entry| entry.path.len())
            .context("evidence path not found")?;
        let local = if entry.path.is_empty() {
            path.as_str()
        } else {
            &path[entry.path.len() + 1..]
        };
        let repo = repository_path(state, entry.repository_id).await?;
        let kind = git(
            &repo,
            &["cat-file", "-t", &format!("{}:{local}", entry.commit)],
        )
        .await
        .context("evidence must reference an existing snapshot file")?;
        ensure!(kind == b"blob\n", "evidence must reference a snapshot file");
    }
    Ok(())
}

pub async fn inspect_tool(
    state: &AppState,
    snapshot_id: Uuid,
    name: &str,
    args: &Value,
) -> Result<Value> {
    if let Some(origin) = crate::runtime_bridge::source_origin(state, snapshot_id).await? {
        return crate::runtime_bridge::source(origin)?
            .post(
                "/v1/inspect",
                &json!({"snapshot_id":snapshot_id,"name":name,"arguments":args}),
            )
            .await;
    }
    if let Some(client) = crate::local_source::client(state).await? {
        return client.post("/internal/local-source", &json!({"operation":"inspect","snapshot_id":snapshot_id,"name":name,"arguments":args})).await;
    }
    let snapshot = load(state, snapshot_id).await?;
    let requested = args
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end_matches('/');
    if !requested.is_empty() {
        validate_path(requested)?;
    }
    if name == "git_metadata" {
        return Ok(
            json!({"snapshot_id":snapshot.id,"commit":snapshot.commit,"repositories":snapshot.repositories}),
        );
    }
    if name == "read_file" {
        let entry = snapshot
            .repositories
            .iter()
            .filter(|entry| {
                entry.path.is_empty() || requested.starts_with(&format!("{}/", entry.path))
            })
            .max_by_key(|entry| entry.path.len())
            .context("source path not found")?;
        let local = if entry.path.is_empty() {
            requested
        } else {
            requested.strip_prefix(&format!("{}/", entry.path)).unwrap()
        };
        validate_path(local)?;
        let repo = repository_path(state, entry.repository_id).await?;
        let object = format!("{}:{local}", entry.commit);
        let size: u64 = String::from_utf8(git(&repo, &["cat-file", "-s", &object]).await?)?
            .trim()
            .parse()?;
        ensure!(
            size <= 256 * 1024,
            "file exceeds 256 KiB; inspect metadata or a smaller file"
        );
        let mut data = git(&repo, &["show", &object]).await?;
        if let Some(item) = entry.lfs.iter().find(|item| item.path == local) {
            ensure!(item.size <= 256 * 1024, "LFS file exceeds 256 KiB");
            data = tokio::fs::read(lfs_path(&repo, &item.oid)?).await?;
        }
        return Ok(
            json!({"path":requested,"content":String::from_utf8(data).context("binary file: use metadata instead")?}),
        );
    }
    if name == "list_tree" {
        let mut files = Vec::new();
        for entry in &snapshot.repositories {
            let repo = repository_path(state, entry.repository_id).await?;
            for (mode, path, _, size) in tree(&repo, &entry.commit).await? {
                let path = if entry.path.is_empty() {
                    path
                } else {
                    format!("{}/{path}", entry.path)
                };
                if requested.is_empty()
                    || path == requested
                    || path.starts_with(&format!("{requested}/"))
                {
                    files.push(json!({"path":path,"mode":mode,"size":size}));
                }
            }
        }
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let total = files.len();
        return Ok(
            json!({"total":total,"files":files.into_iter().skip(offset).take(300).collect::<Vec<_>>(),"next_offset":if offset+300<total {Some(offset+300)}else{None}}),
        );
    }
    if name == "search_source" {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .context("query is required")?;
        ensure!(
            !query.is_empty() && query.len() <= 512 && !query.contains('\0'),
            "invalid search query"
        );
        let mut matches = Vec::new();
        for entry in &snapshot.repositories {
            let repo = repository_path(state, entry.repository_id).await?;
            if let Ok(raw) = git(
                &repo,
                &[
                    "grep",
                    "-n",
                    "-I",
                    "-F",
                    "-m",
                    "10",
                    "-e",
                    query,
                    &entry.commit,
                    "--",
                ],
            )
            .await
            {
                for line in String::from_utf8(raw)?.lines().take(200 - matches.len()) {
                    matches.push(json!({"repository_path":entry.path,"match":line.chars().take(2000).collect::<String>()}));
                }
            }
            if matches.len() >= 200 {
                break;
            }
        }
        return Ok(json!({"matches":matches,"limit":200}));
    }
    bail!("unknown source tool")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn git_relative_urls_resolve_against_repository_directory() {
        assert_eq!(
            resolve_submodule("https://example.com/org/app.git", "../lib.git").unwrap(),
            "https://example.com/org/lib.git"
        );
        assert_eq!(
            resolve_submodule("git@example.com:org/app.git", "../lib.git").unwrap(),
            "ssh://git@example.com/org/lib.git"
        );
        assert!(resolve_submodule("https://example.com/app.git", "file:///etc/passwd").is_err());
    }
    #[test]
    fn lfs_pointer_requires_valid_digest_and_size() {
        assert!(
            pointer(
                "x",
                b"version https://git-lfs.github.com/spec/v1\noid sha256:../bad\nsize 1\n"
            )
            .is_err()
        );
        assert!(pointer("x", b"ordinary file").unwrap().is_none());
    }
}
