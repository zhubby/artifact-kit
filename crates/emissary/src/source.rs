use crate::{
    config::{Config, private_dir},
    process,
    protocol::{Assignment, SourceRepository},
    worker::{Emitter, Event},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

pub fn build_environment() -> BTreeMap<OsString, OsString> {
    [
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "LANG",
        "LC_ALL",
        "TZ",
        "TMPDIR",
        "TMP",
        "TEMP",
        "SystemRoot",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "JAVA_HOME",
        "ANDROID_HOME",
        "ANDROID_SDK_ROOT",
        "DEVELOPER_DIR",
        "SDKROOT",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "GOPATH",
        "GOCACHE",
        "CC",
        "CXX",
        "PKG_CONFIG_PATH",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "NODE_EXTRA_CA_CERTS",
    ]
    .into_iter()
    .filter_map(|name| std::env::var_os(name).map(|v| (name.into(), v)))
    .collect()
}
pub const NULL: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };
pub fn git_environment(cfg: &Config) -> BTreeMap<OsString, OsString> {
    let mut env = build_environment();
    for (key, value) in [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", NULL),
        ("GIT_LFS_SKIP_SMUDGE", "1"),
    ] {
        env.insert(key.into(), value.into());
    }
    if !cfg.git_ca_file.as_os_str().is_empty() {
        env.insert(
            "GIT_SSL_CAINFO".into(),
            cfg.git_ca_file.clone().into_os_string(),
        );
    }
    env
}
pub fn valid_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|u| u.to_string() == id)
}
pub fn valid_commit(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64)
        && commit
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
pub fn managed_path(root: &Path, relative: &str) -> Result<PathBuf> {
    if relative.is_empty() {
        return Ok(root.to_path_buf());
    }
    ensure!(
        !relative.starts_with('/') && !relative.contains(['\\', ':', '\0', '\r', '\n']),
        "unsafe source path"
    );
    let mut path = root.to_path_buf();
    for part in relative.split('/') {
        ensure!(
            !matches!(part, "" | "." | "..") && !part.eq_ignore_ascii_case(".git"),
            "unsafe source path"
        );
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(meta) => ensure!(
                !meta.file_type().is_symlink(),
                "source path crosses a symbolic link"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(path)
}
pub async fn lock(path: &Path, cancel: &CancellationToken, timeout: Duration) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
        tokio::select! {
            _ = cancel.cancelled() => bail!("source cache lock canceled"),
            _ = tokio::time::sleep_until(deadline) => bail!("source cache lock timed out"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {},
        }
    }
}
fn git(cfg: &Config, env: &BTreeMap<OsString, OsString>, args: &[&str]) -> Command {
    let mut command = Command::new(cfg.tool("git"));
    command
        .env_clear()
        .envs(env)
        .args([
            "-c",
            "credential.helper=",
            "-c",
            &format!("core.hooksPath={NULL}"),
            "-c",
            "http.followRedirects=false",
        ])
        .args(args);
    command
}
async fn git_output(
    cfg: &Config,
    env: &BTreeMap<OsString, OsString>,
    args: &[&str],
    cancel: &CancellationToken,
) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    process::output(git(cfg, env, args), cancel, Duration::from_secs(600)).await
}
async fn run_git(
    cfg: &Config,
    env: &BTreeMap<OsString, OsString>,
    args: &[&str],
    cancel: &CancellationToken,
    emit: &Emitter,
) -> Result<()> {
    let (status, output) = git_output(cfg, env, args, cancel)
        .await
        .context("platform source operation interrupted")?;
    for chunk in output.chunks(32 * 1024) {
        emit.send(Event {
            kind: "log".into(),
            step: "checkout".into(),
            data: chunk.to_vec(),
            ..Default::default()
        })
        .await?;
    }
    ensure!(
        status.success(),
        "platform source operation failed; check source access, Git/LFS tools and reachability"
    );
    Ok(())
}
fn path_text(path: &Path) -> Result<&str> {
    path.to_str().context("Git path is not UTF-8")
}
pub async fn checkout(
    cfg: &Config,
    assignment: &Assignment,
    base: &Path,
    workspace: &Path,
    cancel: &CancellationToken,
    emit: &Emitter,
) -> Result<()> {
    let Some(source) = &assignment.source else {
        return checkout_direct(cfg, assignment, base, workspace, cancel, emit).await;
    };
    ensure!(
        valid_id(&source.snapshot_id)
            && source.access_token.len() >= 32
            && !source.repositories.is_empty(),
        "invalid source descriptor"
    );
    let mut env = git_environment(cfg);
    env.insert(
        "GIT_ASKPASS".into(),
        std::env::current_exe()?.into_os_string(),
    );
    env.insert("ARTIFACTORY_GIT_ASKPASS".into(), "1".into());
    env.insert("ARTIFACTORY_GIT_USERNAME".into(), "artifactory".into());
    env.insert(
        "ARTIFACTORY_GIT_PASSWORD".into(),
        source.access_token.clone().into(),
    );
    let mut repos = source.repositories.iter().collect::<Vec<_>>();
    repos.sort_by_key(|r| &r.path);
    ensure!(
        repos[0].path.is_empty() && repos[0].commit_sha == assignment.commit_sha,
        "source root does not match pinned commit"
    );
    let cache_root = cfg.root.join("cache/git");
    private_dir(&cache_root)?;
    let mut seen = HashSet::new();
    for repo in &repos {
        ensure!(
            valid_id(&repo.repository_id)
                && valid_id(&repo.cache_id)
                && valid_commit(&repo.commit_sha)
                && seen.insert(&repo.path),
            "invalid source repository"
        );
        validate_url(&repo.git_url, cfg.allow_http_uploads)?;
        let target = managed_path(workspace, &repo.path)?;
        let _lock = lock(
            &cache_root.join(format!("{}.lock", repo.cache_id)),
            cancel,
            Duration::from_secs(600),
        )
        .await?;
        checkout_repository(cfg, repo, &env, &cache_root, &target, cancel, emit).await?;
    }
    for parent in &repos {
        let target = managed_path(workspace, &parent.path)?;
        if !target.join(".gitmodules").exists() {
            continue;
        }
        let (status, data) = git_output(
            cfg,
            &env,
            &[
                "-C",
                path_text(&target)?,
                "config",
                "--null",
                "--file",
                ".gitmodules",
                "--get-regexp",
                r"^submodule\..*\.path$",
            ],
            cancel,
        )
        .await?;
        ensure!(
            status.success() || status.code() == Some(1),
            "cannot read snapshot submodule paths"
        );
        for item in data.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            let item = std::str::from_utf8(item)?;
            let (key, relative) = item
                .split_once('\n')
                .context("invalid submodule path entry")?;
            managed_path(&target, relative)?;
            let child_path = if parent.path.is_empty() {
                relative.into()
            } else {
                format!("{}/{relative}", parent.path)
            };
            if let Some(child) = repos.iter().find(|r| r.path == child_path) {
                let url_key = format!(
                    "{}.url",
                    key.strip_suffix(".path").context("invalid submodule key")?
                );
                run_git(
                    cfg,
                    &env,
                    &[
                        "-C",
                        path_text(&target)?,
                        "config",
                        "--local",
                        &url_key,
                        &child.git_url,
                    ],
                    cancel,
                    emit,
                )
                .await?;
            }
        }
    }
    let _ = collect_cache(cfg, cancel).await;
    Ok(())
}
async fn checkout_repository(
    cfg: &Config,
    repo: &SourceRepository,
    env: &BTreeMap<OsString, OsString>,
    cache_root: &Path,
    target: &Path,
    cancel: &CancellationToken,
    emit: &Emitter,
) -> Result<()> {
    let cache = cache_root.join(format!("{}.git", repo.cache_id));
    let c = path_text(&cache)?;
    let t = path_text(target)?;
    if !cache.join("HEAD").exists() {
        run_git(cfg, env, &["init", "--bare", c], cancel, emit).await?;
    }
    let mut cached = git_output(
        cfg,
        env,
        &[
            "-C",
            c,
            "cat-file",
            "-e",
            &format!("{}^{{commit}}", repo.commit_sha),
        ],
        cancel,
    )
    .await?
    .0
    .success();
    for oid in repo.tags.values() {
        ensure!(valid_commit(oid), "invalid source tag object");
        if cached
            && !git_output(cfg, env, &["-C", c, "cat-file", "-e", oid], cancel)
                .await?
                .0
                .success()
        {
            cached = false;
        }
    }
    if !cached {
        run_git(
            cfg,
            env,
            &[
                "-C",
                c,
                "fetch",
                "--force",
                "--tags",
                "--no-recurse-submodules",
                "--",
                &repo.git_url,
                "refs/heads/source",
            ],
            cancel,
            emit,
        )
        .await?;
    }
    emit.send(Event {
        kind: "source_cache".into(),
        payload: serde_json::json!({"path":repo.path,"hit":cached,"commit":repo.commit_sha}),
        ..Default::default()
    })
    .await?;
    run_git(
        cfg,
        env,
        &["-C", c, "update-ref", "refs/heads/source", &repo.commit_sha],
        cancel,
        emit,
    )
    .await?;
    if repo.has_lfs {
        ensure!(
            repo.lfs_url == format!("{}/lfs", repo.git_url.trim_end_matches("/git")),
            "invalid platform LFS endpoint"
        );
        run_git(
            cfg,
            env,
            &[
                "-C",
                c,
                "-c",
                &format!("remote.origin.url={}", repo.git_url),
                "-c",
                &format!("lfs.url={}", repo.lfs_url),
                "lfs",
                "fetch",
                "origin",
                &repo.commit_sha,
            ],
            cancel,
            emit,
        )
        .await?;
    }
    private_dir(target.parent().context("source parent missing")?)?;
    run_git(
        cfg,
        env,
        &[
            "clone",
            "--local",
            "--no-hardlinks",
            "--no-checkout",
            "--",
            c,
            t,
        ],
        cancel,
        emit,
    )
    .await?;
    run_git(
        cfg,
        env,
        &["-C", t, "remote", "set-url", "origin", &repo.git_url],
        cancel,
        emit,
    )
    .await?;
    let (status, tags) = git_output(cfg, env, &["-C", t, "tag", "--list"], cancel).await?;
    ensure!(status.success(), "cannot read local tags");
    for name in String::from_utf8(tags)?.lines() {
        run_git(
            cfg,
            env,
            &["-C", t, "update-ref", "-d", &format!("refs/tags/{name}")],
            cancel,
            emit,
        )
        .await?;
    }
    for (name, oid) in &repo.tags {
        ensure!(!name.starts_with('-'), "invalid frozen tag");
        run_git(
            cfg,
            env,
            &["-C", t, "update-ref", &format!("refs/tags/{name}"), oid],
            cancel,
            emit,
        )
        .await?;
    }
    if repo.has_lfs {
        copy_lfs(&cache.join("lfs/objects"), &target.join(".git/lfs/objects"))?;
        for (key, value) in [
            ("lfs.url", repo.lfs_url.as_str()),
            ("filter.lfs.clean", "git-lfs clean -- %f"),
            ("filter.lfs.smudge", "git-lfs smudge -- %f"),
            ("filter.lfs.process", "git-lfs filter-process"),
            ("filter.lfs.required", "true"),
        ] {
            run_git(cfg, env, &["-C", t, "config", key, value], cancel, emit).await?;
        }
    }
    run_git(
        cfg,
        env,
        &["-C", t, "checkout", "--detach", &repo.commit_sha],
        cancel,
        emit,
    )
    .await?;
    if repo.has_lfs {
        run_git(cfg, env, &["-C", t, "lfs", "checkout"], cancel, emit).await?;
        run_git(
            cfg,
            env,
            &["-C", t, "lfs", "fsck", "--objects", &repo.commit_sha],
            cancel,
            emit,
        )
        .await?;
    }
    touch_cache(&cache)?;
    Ok(())
}
fn touch_cache(path: &Path) -> Result<()> {
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_WRITE_ATTRIBUTES,
        };
        OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?
    };
    #[cfg(not(windows))]
    let file = File::open(path)?;
    file.set_modified(SystemTime::now())?;
    Ok(())
}
pub fn copy_lfs(source: &Path, destination: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(source).follow_links(false) {
        let entry = entry?;
        let target = destination.join(entry.path().strip_prefix(source)?);
        if entry.file_type().is_dir() {
            private_dir(&target)?;
        } else {
            ensure!(entry.file_type().is_file(), "invalid LFS cache entry");
            let relative = entry
                .path()
                .strip_prefix(source)?
                .to_str()
                .context("invalid LFS path")?
                .replace('\\', "/");
            let mut input = crate::artifacts::open(source, &relative)?;
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            std::io::copy(&mut input, &mut options.open(target)?)?;
        }
    }
    Ok(())
}
pub async fn verify(
    cfg: &Config,
    assignment: &Assignment,
    workspace: &Path,
    cancel: &CancellationToken,
) -> Result<()> {
    if let Some(source) = &assignment.source {
        let env = git_environment(cfg);
        for repo in &source.repositories {
            let target = managed_path(workspace, &repo.path)?;
            let t = path_text(&target)?;
            ensure!(
                git_output(
                    cfg,
                    &env,
                    &[
                        "-c",
                        "core.filemode=true",
                        "-c",
                        "core.fsmonitor=false",
                        "-C",
                        t,
                        "diff",
                        "--exit-code",
                        "--no-ext-diff",
                        "--ignore-submodules=untracked",
                        "HEAD",
                        "--"
                    ],
                    cancel
                )
                .await?
                .0
                .success(),
                "source_modified: tracked files or submodule state changed in {:?}; use external build output",
                repo.path
            );
            let (status, head) =
                git_output(cfg, &env, &["-C", t, "rev-parse", "HEAD"], cancel).await?;
            ensure!(
                status.success() && String::from_utf8_lossy(&head).trim() == repo.commit_sha,
                "source_modified: pinned source commit changed"
            );
        }
    }
    Ok(())
}
pub fn validate_url(value: &str, allow_http: bool) -> Result<url::Url> {
    let url = url::Url::parse(value)?;
    ensure!(
        (url.scheme() == "https" || (allow_http && url.scheme() == "http"))
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none(),
        "HTTPS URL without embedded credentials required"
    );
    Ok(url)
}
async fn checkout_direct(
    cfg: &Config,
    assignment: &Assignment,
    base: &Path,
    workspace: &Path,
    cancel: &CancellationToken,
    emit: &Emitter,
) -> Result<()> {
    ensure!(
        valid_commit(&assignment.commit_sha),
        "invalid pinned commit"
    );
    let auth = tempfile::Builder::new().prefix("auth-").tempdir_in(base)?;
    let mut env = git_environment(cfg);
    if let Some(credential) = &assignment.git_credential {
        match credential.kind.as_str() {
            "https" => {
                env.insert(
                    "GIT_ASKPASS".into(),
                    std::env::current_exe()?.into_os_string(),
                );
                env.insert("ARTIFACTORY_GIT_ASKPASS".into(), "1".into());
                env.insert(
                    "ARTIFACTORY_GIT_USERNAME".into(),
                    credential.username.clone().into(),
                );
                env.insert(
                    "ARTIFACTORY_GIT_PASSWORD".into(),
                    credential.secret.clone().into(),
                );
            }
            "ssh" => {
                ensure!(
                    !credential.known_hosts.is_empty(),
                    "SSH host trust is required"
                );
                let key = auth.path().join("identity");
                let hosts = auth.path().join("known_hosts");
                crate::config::private_write(&key, credential.secret.as_bytes(), false, true)?;
                crate::config::private_write(
                    &hosts,
                    credential.known_hosts.as_bytes(),
                    false,
                    true,
                )?;
                let trusted = format!(
                    "UserKnownHostsFile=\"{}\"",
                    path_text(&hosts)?
                        .replace('\\', "/")
                        .replace('"', "\\\"")
                        .replace('%', "%%")
                );
                env.insert("GIT_SSH_COMMAND".into(),format!("ssh -F {} -i {} -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o {}",quote(NULL),quote(path_text(&key)?),quote(&trusted)).into());
            }
            _ => bail!("unsupported Git credential"),
        }
    }
    let w = path_text(workspace)?;
    for args in [
        vec!["init", w],
        vec![
            "-C",
            w,
            "fetch",
            "--depth=1",
            "--no-tags",
            "--",
            &assignment.repository_url,
            &assignment.commit_sha,
        ],
        vec!["-C", w, "checkout", "--detach", &assignment.commit_sha],
    ] {
        tokio::time::timeout(
            Duration::from_secs(120),
            run_git(cfg, &env, &args, cancel, emit),
        )
        .await??;
    }
    Ok(())
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
async fn collect_cache(cfg: &Config, cancel: &CancellationToken) -> Result<()> {
    let root = cfg.root.join("cache/git");
    let mut caches = Vec::new();
    let mut total = 0;
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(id) = name.strip_suffix(".git").filter(|id| valid_id(id)) else {
            continue;
        };
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let size = walkdir::WalkDir::new(entry.path())
            .follow_links(false)
            .into_iter()
            .map(|e| e.map(|e| e.metadata().map(|m| if m.is_file() { m.len() } else { 0 })))
            .try_fold(0u64, |sum, item| Ok::<_, anyhow::Error>(sum + item??))?;
        total += size;
        caches.push((
            entry.metadata()?.modified()?,
            entry.path(),
            id.to_string(),
            size,
        ));
    }
    caches.sort_by_key(|c| c.0);
    let quota = if cfg.source_cache_bytes == 0 {
        20 << 30
    } else {
        cfg.source_cache_bytes
    };
    for (_, path, id, size) in caches {
        if total <= quota {
            break;
        }
        if let Ok(_guard) = lock(
            &root.join(format!("{id}.lock")),
            cancel,
            Duration::from_millis(50),
        )
        .await
        {
            fs::remove_dir_all(path)?;
            total -= size;
        }
    }
    Ok(())
}
