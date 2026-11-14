use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub endpoint: String,
    pub telemetry_endpoint: String,
    pub telemetry_buffer_bytes: u64,
    pub ca_file: PathBuf,
    pub certificate_file: PathBuf,
    pub key_file: PathBuf,
    pub emissary_id: String,
    pub name: String,
    pub root: PathBuf,
    pub git_ca_file: PathBuf,
    pub tools: BTreeMap<String, String>,
    /// Custom startup values; keys are automatically prefixed with custom.
    pub metadata: BTreeMap<String, serde_json::Value>,
    pub signing_profiles: BTreeMap<String, BTreeMap<String, String>>,
    pub log_buffer_bytes: u64,
    pub source_cache_bytes: u64,
    pub allow_http_uploads: bool,
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut cfg: Self =
            serde_json::from_reader(fs::File::open(path).context("open Emissary configuration")?)?;
        ensure!(
            !cfg.endpoint.is_empty()
                && !cfg.ca_file.as_os_str().is_empty()
                && !cfg.root.as_os_str().is_empty(),
            "endpoint, CA file and managed root are required"
        );
        cfg.root = std::path::absolute(cfg.root)?;
        if cfg.log_buffer_bytes == 0 {
            cfg.log_buffer_bytes = 256 << 20;
        }
        if cfg.source_cache_bytes == 0 {
            cfg.source_cache_bytes = 20 << 30;
        }
        if cfg.telemetry_buffer_bytes == 0 {
            cfg.telemetry_buffer_bytes = 32 << 20;
        }
        ensure!(
            cfg.telemetry_buffer_bytes >= 1 << 20,
            "telemetry buffer must be at least 1 MiB"
        );
        ensure!(
            cfg.log_buffer_bytes >= 1 << 20,
            "log buffer must be at least 1 MiB"
        );
        ensure!(
            cfg.source_cache_bytes >= 1 << 20,
            "source cache quota must be at least 1 MiB"
        );
        Ok(cfg)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        atomic_json(path, self)
    }
    pub fn tool(&self, name: &str) -> String {
        self.tools.get(name).cloned().unwrap_or_else(|| name.into())
    }
}
pub fn private_dir(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    Ok(())
}
pub fn private_write(path: &Path, bytes: &[u8], executable: bool, exclusive: bool) -> Result<()> {
    // Windows has no Unix execute permission bit.
    #[cfg(windows)]
    let _ = executable;
    let mut options = fs::OpenOptions::new();
    options
        .write(true)
        .create_new(exclusive)
        .create(!exclusive)
        .truncate(!exclusive);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable { 0o700 } else { 0o600 });
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    atomic_bytes(path, &serde_json::to_vec(value)?, false)
}
pub fn atomic_bytes(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    #[cfg(windows)]
    let _ = executable;
    let parent = path.parent().context("path requires parent")?;
    private_dir(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(if executable {
                0o700
            } else {
                0o600
            }))?;
    }
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let file = fs::File::open(path)?;
    ensure!(
        file.metadata()?.len() <= 131_072,
        "oversized private record"
    );
    Ok(serde_json::from_reader(file)?)
}
