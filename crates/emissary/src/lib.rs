//! Artifactory device runtime: isolated builds, leases, logs, artifacts and OTA.
pub use artifactory_emissary_protocol as protocol;
pub mod artifacts;
pub mod client;
pub mod config;
pub mod doctor;
pub mod ota;
pub mod process;
pub mod runner;
pub mod service;
pub mod source;
pub mod telemetry;
pub mod worker;

pub const VERSION: &str = match option_env!("ARTIFACTORY_BUILD_VERSION") {
    Some(value) => value,
    None => env!("CARGO_PKG_VERSION"),
};
pub const COMMIT: &str = match option_env!("ARTIFACTORY_BUILD_COMMIT") {
    Some(value) => value,
    None => "development",
};
pub fn platform() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        std::env::consts::OS
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    (os, arch)
}
pub async fn run_configured(
    path: &std::path::Path,
    cfg: config::Config,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    if std::env::var_os("ARTIFACTORY_OTA_DIRECTORY").is_none() {
        let ota_path = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("configuration parent missing"))?
            .join("ota.json");
        if ota_path.try_exists()? {
            return ota::supervise(path, VERSION, cancel).await;
        }
    }
    runner::run(cfg, cancel).await
}
