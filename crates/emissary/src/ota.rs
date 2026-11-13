use crate::{
    config::{atomic_json, private_dir, read_json},
    process, source,
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use futures::StreamExt;
use process_wrap::tokio::ChildWrapper;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{process::Command, task::JoinHandle};
use tokio_util::sync::CancellationToken;

pub const MAX_BINARY_SIZE: u64 = 128 << 20;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub os: String,
    pub arch: String,
    pub name: String,
    pub size: u64,
    pub sha256: String,
}
impl Asset {
    pub fn from_bytes(os: &str, arch: &str, bytes: &[u8]) -> Self {
        Self {
            os: os.into(),
            arch: arch.into(),
            name: asset_name(os, arch),
            size: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(bytes)),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub commit: String,
    pub protocol_min: u32,
    pub protocol_max: u32,
    pub assets: Vec<Asset>,
}
pub fn asset_name(os: &str, arch: &str) -> String {
    format!(
        "artifactory-emissary-{os}-{arch}{}",
        if os == "windows" { ".exe" } else { "" }
    )
}
pub fn binary_name() -> &'static str {
    if cfg!(windows) {
        "artifactory-emissary.exe"
    } else {
        "artifactory-emissary"
    }
}
pub fn safe_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 100
        && version.as_bytes()[0].is_ascii_alphanumeric()
        && version
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}
pub fn verify(raw: &[u8], signature: &str, keys: &[String], os: &str, arch: &str) -> Result<Asset> {
    ensure!(raw.len() <= 65536, "oversized manifest");
    let signature = Signature::from_slice(
        &STANDARD
            .decode(signature)
            .context("invalid signature encoding")?,
    )?;
    let valid = keys.iter().any(|key| {
        STANDARD
            .decode(key)
            .ok()
            .and_then(|raw| <[u8; 32]>::try_from(raw).ok())
            .and_then(|bytes| VerifyingKey::from_bytes(&bytes).ok())
            .is_some_and(|key| key.verify(raw, &signature).is_ok())
    });
    ensure!(valid, "manifest signature rejected");
    let manifest: Manifest = serde_json::from_slice(raw)?;
    ensure!(
        safe_version(&manifest.version)
            && manifest.commit.len() == 40
            && manifest.protocol_min == 1
            && manifest.protocol_max >= 1
            && !manifest.assets.is_empty()
            && manifest.assets.len() <= 6,
        "incompatible release manifest"
    );
    let mut seen = std::collections::HashSet::new();
    let mut result = None;
    for asset in manifest.assets {
        let platform = format!("{}/{}", asset.os, asset.arch);
        ensure!(
            matches!(
                platform.as_str(),
                "linux/amd64"
                    | "linux/arm64"
                    | "darwin/amd64"
                    | "darwin/arm64"
                    | "windows/amd64"
                    | "windows/arm64"
            ) && seen.insert(platform)
                && asset.name == asset_name(&asset.os, &asset.arch)
                && asset.size > 0
                && asset.size <= MAX_BINARY_SIZE
                && asset.sha256.len() == 64
                && asset
                    .sha256
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "invalid release asset"
        );
        if asset.os == os && asset.arch == arch {
            result = Some(asset);
        }
    }
    result.with_context(|| format!("release has no compatible {os}/{arch} binary"))
}
pub fn verify_bytes(bytes: &[u8], asset: &Asset) -> Result<()> {
    ensure!(
        bytes.len() as u64 == asset.size && hex::encode(Sha256::digest(bytes)) == asset.sha256,
        "binary size or SHA-256 mismatch"
    );
    Ok(())
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct OtaConfig {
    pub directory: PathBuf,
    pub public_keys: Vec<String>,
    #[serde(default)]
    pub allow_http: bool,
}
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Image {
    pub version: String,
    pub path: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub activation_pending: bool,
    pub generation: i64,
    pub update_id: String,
    pub target: String,
    pub phase: String,
    pub error: String,
    pub current: Image,
    pub previous: Image,
    pub candidate: Image,
    pub launcher_version: String,
    pub started_at: DateTime<Utc>,
}
impl State {
    pub fn recover(&mut self) -> bool {
        match self.phase.as_str() {
            "activating" => {
                self.phase = "rolled_back".into();
                self.error =
                    "launcher interrupted during activation; restored previous runtime".into();
            }
            "downloading" | "draining" => {
                self.phase = "failed".into();
                self.error =
                    "launcher interrupted before activation; current runtime preserved".into();
            }
            _ => return false,
        }
        self.candidate = Image::default();
        self.activation_pending = false;
        true
    }
    pub fn active(&self) -> bool {
        matches!(
            self.phase.as_str(),
            "downloading" | "draining" | "activating"
        )
    }
    pub fn request(&mut self, request: &Request) -> bool {
        if request.id.is_empty() || request.id.len() > 100 {
            return false;
        }
        if request.action == "cancel" {
            if request.generation > self.generation && !self.active() {
                self.generation = request.generation;
                self.update_id = request.id.clone();
            } else if request.id != self.update_id
                || request.generation != self.generation
                || !matches!(self.phase.as_str(), "downloading" | "draining")
            {
                return false;
            }
            self.phase = "canceled".into();
            self.error.clear();
            self.candidate = Image::default();
            return true;
        }
        if request.action == "apply" && request.generation > self.generation && !self.active() {
            self.generation = request.generation;
            self.update_id = request.id.clone();
            self.error.clear();
            self.phase = "downloading".into();
            self.started_at = Utc::now();
            self.candidate = Image::default();
            self.target = serde_json::from_str::<Manifest>(&request.manifest)
                .map(|m| m.version)
                .unwrap_or_default();
            return true;
        }
        false
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Request {
    pub generation: i64,
    #[serde(rename = "id")]
    pub id: String,
    pub action: String,
    pub manifest: String,
    pub signature: String,
    pub url: String,
}
#[derive(Serialize, Deserialize)]
pub struct Health {
    pub session: String,
    pub id: String,
    pub time: DateTime<Utc>,
}
#[derive(Default)]
pub struct HealthWindow {
    session: String,
    last: String,
    count: u32,
    at: Option<Instant>,
}
impl HealthWindow {
    pub fn ack(&mut self, session: &str, id: &str, now: Instant) -> bool {
        if self.session != session
            || self
                .at
                .is_some_and(|at| now.saturating_duration_since(at) > Duration::from_secs(25))
            || id.is_empty()
        {
            *self = Self {
                session: session.into(),
                ..Default::default()
            };
        }
        if !id.is_empty() && id != self.last {
            self.last = id.into();
            self.count += 1;
            self.at = Some(now);
        }
        self.count >= 3
    }
}
pub fn load(path: &Path) -> Result<OtaConfig> {
    let cfg: OtaConfig = read_json(
        &path
            .parent()
            .context("configuration parent missing")?
            .join("ota.json"),
    )?;
    ensure!(
        cfg.directory.is_absolute() && !cfg.public_keys.is_empty(),
        "invalid OTA configuration"
    );
    Ok(cfg)
}
pub fn snapshot(directory: &Path) -> Result<State> {
    read_json(&directory.join("state.json"))
}
pub fn submit(directory: &Path, request: &Request) -> Result<()> {
    atomic_json(&directory.join("request.json"), request)
}
pub fn confirm(directory: &Path, session: &str, id: &str) -> Result<()> {
    atomic_json(
        &directory.join("health.json"),
        &Health {
            session: session.into(),
            id: id.into(),
            time: Utc::now(),
        },
    )
}
pub fn initialize(
    config_path: &Path,
    directory: &Path,
    binary: &Path,
    version: &str,
    keys: Vec<String>,
    allow_http: bool,
) -> Result<()> {
    ensure!(
        directory.is_absolute() && safe_version(version) && !keys.is_empty(),
        "invalid bootstrap options"
    );
    for key in &keys {
        ensure!(STANDARD.decode(key)?.len() == 32, "invalid OTA public key");
    }
    let ota_path = config_path
        .parent()
        .context("configuration parent missing")?
        .join("ota.json");
    ensure!(!ota_path.try_exists()?, "OTA already configured");
    private_dir(directory)?;
    let body = fs::read(binary)?;
    let target = directory
        .join("versions")
        .join(hex::encode(Sha256::digest(&body)))
        .join(binary_name());
    private_dir(target.parent().context("image parent missing")?)?;
    crate::config::atomic_bytes(&target, &body, true)?;
    atomic_json(
        &directory.join("state.json"),
        &State {
            current: Image {
                version: version.into(),
                path: target.to_string_lossy().into(),
            },
            phase: "idle".into(),
            launcher_version: version.into(),
            ..Default::default()
        },
    )?;
    atomic_json(
        &ota_path,
        &OtaConfig {
            directory: directory.into(),
            public_keys: keys,
            allow_http,
        },
    )
}
pub async fn download(
    cfg: &OtaConfig,
    request: &Request,
    cancel: &CancellationToken,
) -> Result<Image> {
    let (os, arch) = crate::platform();
    let asset = verify(
        request.manifest.as_bytes(),
        &request.signature,
        &cfg.public_keys,
        os,
        arch,
    )?;
    let manifest: Manifest = serde_json::from_str(&request.manifest)?;
    let url = source::validate_url(&request.url, cfg.allow_http)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let operation = async {
        let response = client
            .get(url)
            .send()
            .await
            .context("release download interrupted")?;
        ensure!(
            response.status() == reqwest::StatusCode::OK,
            "release download returned HTTP {}",
            response.status()
        );
        let directory = cfg.directory.join("versions").join(&asset.sha256);
        private_dir(&directory)?;
        let mut file = tempfile::NamedTempFile::new_in(&directory)?;
        let mut body = response.bytes_stream();
        let mut size = 0u64;
        let mut hash = Sha256::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.context("release download interrupted")?;
            size += chunk.len() as u64;
            ensure!(size <= asset.size, "binary size or SHA-256 mismatch");
            hash.update(&chunk);
            file.write_all(&chunk)?;
        }
        ensure!(
            size == asset.size && hex::encode(hash.finalize()) == asset.sha256,
            "binary size or SHA-256 mismatch"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o700))?;
        }
        file.as_file().sync_all()?;
        let target = directory.join(binary_name());
        if target.try_exists()? {
            verify_bytes(&fs::read(&target)?, &asset)?;
        } else {
            file.persist_noclobber(&target).map_err(|e| e.error)?;
        }
        Ok(Image {
            version: manifest.version,
            path: target.to_string_lossy().into(),
        })
    };
    tokio::select! {result=operation=>result,_=cancel.cancelled()=>bail!("release download canceled")}
}
struct Runtime {
    child: Box<dyn ChildWrapper>,
}
impl Runtime {
    fn start(config_path: &Path, cfg: &OtaConfig, path: &str, version: &str) -> Result<Self> {
        let mut command = Command::new(path);
        command
            .args(["run", "--config"])
            .arg(config_path)
            .env("ARTIFACTORY_OTA_DIRECTORY", &cfg.directory)
            .env("ARTIFACTORY_OTA_LAUNCHER_VERSION", version)
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        Ok(Self {
            child: process::spawn(command)?,
        })
    }
    async fn stop(&mut self) {
        self.child.stdin().take();
        if tokio::time::timeout(Duration::from_secs(8), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
    }
}
pub async fn supervise(config_path: &Path, version: &str, cancel: CancellationToken) -> Result<()> {
    let cfg = load(config_path)?;
    let _lock = source::lock(
        &cfg.directory.join("launcher.lock"),
        &cancel,
        Duration::from_millis(1),
    )
    .await?;
    let mut state = snapshot(&cfg.directory)?;
    state.recover();
    state.launcher_version = version.into();
    let path = cfg.directory.join("state.json");
    atomic_json(&path, &state)?;
    let mut current = Runtime::start(config_path, &cfg, &state.current.path, version)?;
    let mut download_task: Option<JoinHandle<(String, Result<Image>)>> = None;
    let mut download_cancel = cancel.child_token();
    let mut health = HealthWindow::default();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let result = async {
        loop {
            tokio::select! {
                _=cancel.cancelled()=>return Ok(()),
                _=tick.tick()=>{},
            }
            if let Some(status) = current.child.try_wait()? {
                if state.phase == "activating" {
                    state.phase = "rolled_back".into();
                    state.error = format!("candidate exited before health confirmation ({status})");
                    state.candidate = Image::default();
                    atomic_json(&path, &state)?;
                    current = Runtime::start(config_path, &cfg, &state.current.path, version)?;
                } else {
                    bail!("Emissary runtime exited; service restart required");
                }
            }
            if download_task
                .as_ref()
                .is_some_and(|task| task.is_finished())
            {
                let task = download_task.take().context("missing download task")?;
                let (id, result) = task.await?;
                if state.update_id == id && state.phase == "downloading" {
                    match result {
                        Ok(image) => {
                            state.candidate = image;
                            state.phase = "draining".into();
                        }
                        Err(e) => {
                            state.phase = "failed".into();
                            state.error = e.to_string();
                        }
                    }
                    atomic_json(&path, &state)?;
                }
            }
            let mailbox = cfg.directory.join("request.json");
            if let Ok(request) = read_json::<Request>(&mailbox) {
                fs::remove_file(&mailbox)?;
                if state.request(&request) {
                    atomic_json(&path, &state)?;
                    download_cancel.cancel();
                    if let Some(task) = download_task.take() {
                        task.abort();
                        let _ = task.await;
                    }
                    if state.phase == "downloading" {
                        download_cancel = cancel.child_token();
                        let token = download_cancel.clone();
                        let cfg = cfg.clone();
                        download_task = Some(tokio::spawn(async move {
                            let result = download(&cfg, &request, &token).await;
                            (request.id, result)
                        }));
                    }
                }
            }
            if state.phase == "draining" {
                let ready = read_json::<serde_json::Value>(&cfg.directory.join("drained.json"));
                if ready
                    .ok()
                    .and_then(|v| v.get("id").and_then(|v| v.as_str()).map(str::to_owned))
                    .as_deref()
                    == Some(&state.update_id)
                {
                    current.stop().await;
                    state.phase = "activating".into();
                    state.started_at = Utc::now();
                    health = HealthWindow::default();
                    let _ = fs::remove_file(cfg.directory.join("health.json"));
                    atomic_json(&path, &state)?;
                    match Runtime::start(config_path, &cfg, &state.candidate.path, version) {
                        Ok(runtime) => current = runtime,
                        Err(_) => {
                            state.phase = "rolled_back".into();
                            state.error = "candidate failed to start".into();
                            state.candidate = Image::default();
                            atomic_json(&path, &state)?;
                            current =
                                Runtime::start(config_path, &cfg, &state.current.path, version)?;
                        }
                    }
                }
            }
            if state.phase == "activating" {
                let confirmed = read_json::<Health>(&cfg.directory.join("health.json"))
                    .ok()
                    .filter(|ack| {
                        Utc::now()
                            .signed_duration_since(ack.time)
                            .num_seconds()
                            .abs()
                            < 25
                    })
                    .is_some_and(|ack| health.ack(&ack.session, &ack.id, Instant::now()));
                if confirmed {
                    state.previous = state.current.clone();
                    state.current = std::mem::take(&mut state.candidate);
                    state.phase = "succeeded".into();
                    atomic_json(&path, &state)?;
                } else if Utc::now()
                    .signed_duration_since(state.started_at)
                    .num_seconds()
                    >= 120
                {
                    current.stop().await;
                    state.phase = "rolled_back".into();
                    state.error =
                        "candidate health confirmation timed out after 120 seconds".into();
                    state.candidate = Image::default();
                    atomic_json(&path, &state)?;
                    current = Runtime::start(config_path, &cfg, &state.current.path, version)?;
                }
            }
        }
    }
    .await;
    download_cancel.cancel();
    if let Some(task) = download_task {
        task.abort();
        let _ = task.await;
    }
    current.stop().await;
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancel_overtakes_apply_and_persists_generation_fence() {
        let mut state = State {
            phase: "idle".into(),
            ..Default::default()
        };
        let mut request = Request {
            generation: 9,
            id: "update".into(),
            action: "cancel".into(),
            ..Default::default()
        };
        assert!(state.request(&request));
        request.action = "apply".into();
        assert!(!state.request(&request));
        request.generation = 10;
        assert!(state.request(&request));
        request.action = "cancel".into();
        assert!(state.request(&request));
        assert_eq!(state.generation, 10);
        assert_eq!(state.phase, "canceled");
    }
}
