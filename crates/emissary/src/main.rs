use anyhow::{Context, Result, ensure};
use artifactory_emissary::{self as emissary, config::Config};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "artifactory-emissary",
    about = "Artifactory authenticated device build runtime"
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    Register(Register),
    Run(Common),
    Doctor(Doctor),
    Service(Service),
    Version,
    OtaInit(OtaInit),
    #[command(hide = true)]
    Worker,
    #[command(hide = true)]
    ServiceRun(Common),
}
#[derive(Args)]
struct Common {
    #[arg(long,default_value_os_t=default_config())]
    config: PathBuf,
}
#[derive(Args)]
struct Register {
    #[command(flatten)]
    common: Common,
    #[arg(long)]
    endpoint: String,
    #[arg(long)]
    ca: PathBuf,
    #[arg(long,default_value_os_t=default_root())]
    root: PathBuf,
    #[arg(long)]
    name: Option<String>,
    #[arg(long, default_value = "ARTIFACTORY_ENROLLMENT_TOKEN")]
    token_env: String,
    #[arg(long)]
    allow_http_uploads: bool,
}
#[derive(Args)]
struct Doctor {
    #[command(flatten)]
    common: Common,
    #[arg(long,default_value_os_t=default_root())]
    root: PathBuf,
}
#[derive(Args)]
struct Service {
    #[command(flatten)]
    common: Common,
    #[arg(long)]
    action: String,
    #[arg(long)]
    user: Option<String>,
}
#[derive(Args)]
struct OtaInit {
    #[command(flatten)]
    common: Common,
    #[arg(long, value_delimiter = ',')]
    public_keys: Vec<String>,
}
fn default_config() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.config_dir().join("artifactory/emissary.json"))
        .unwrap_or_default()
}
fn default_root() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.data_local_dir().join("artifactory/emissary"))
        .unwrap_or_default()
}
#[tokio::main]
async fn main() {
    if std::env::var("ARTIFACTORY_GIT_ASKPASS").as_deref() == Ok("1") {
        let prompt = std::env::args()
            .skip(1)
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        println!(
            "{}",
            std::env::var(if prompt.contains("username") {
                "ARTIFACTORY_GIT_USERNAME"
            } else {
                "ARTIFACTORY_GIT_PASSWORD"
            })
            .unwrap_or_default()
        );
        return;
    }
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
    // Tokio's portable stdin uses a blocking reader that cannot be canceled.
    // All workers, process trees and output are settled by run before returning;
    // do not wait for another byte on an otherwise live parent pipe at shutdown.
    std::process::exit(0);
}
async fn run(cli: Cli) -> Result<()> {
    let cancel = CancellationToken::new();
    let signals = cancel.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let Ok(mut term) = signal(SignalKind::terminate()) else {
                return;
            };
            let Ok(mut interrupt) = signal(SignalKind::interrupt()) else {
                return;
            };
            tokio::select! {_=term.recv()=>{},_=interrupt.recv()=>{}}
        }
        #[cfg(windows)]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        signals.cancel();
    });
    match cli.command {
        Action::Version => {
            let (os, arch) = emissary::platform();
            println!(
                "artifactory-emissary {} {os}/{arch} protocol=1 commit={}",
                emissary::VERSION,
                emissary::COMMIT
            );
        }
        Action::Worker => return emissary::worker::stdin_worker(cancel).await,
        Action::Register(args) => {
            ensure!(
                !args.common.config.as_os_str().is_empty() && !args.root.as_os_str().is_empty(),
                "--config and --root are required for accounts without a profile"
            );
            let cfg = Config {
                endpoint: args.endpoint,
                ca_file: std::path::absolute(args.ca)?,
                root: std::path::absolute(args.root)?,
                name: args.name.unwrap_or_else(|| {
                    sysinfo::System::host_name().unwrap_or_else(|| "emissary".into())
                }),
                allow_http_uploads: args.allow_http_uploads,
                ..Default::default()
            };
            emissary::client::enroll(
                &args.common.config,
                cfg,
                std::env::var(args.token_env)
                    .context("enrollment token environment variable missing")?,
            )
            .await?;
            println!(
                "Emissary enrolled; configuration saved to {}",
                args.common.config.display()
            );
        }
        Action::Doctor(args) => {
            let loaded = Config::load(&args.common.config);
            let registered = loaded.as_ref().is_ok_and(|c| !c.emissary_id.is_empty());
            let configuration_loaded = loaded.is_ok();
            let cfg = loaded.unwrap_or_else(|_| Config {
                root: args.root,
                ..Default::default()
            });
            let (mut diagnostics, capabilities) = emissary::doctor::doctor(&cfg).await;
            diagnostics["capabilities"] = serde_json::json!(capabilities);
            diagnostics["registered"] = serde_json::json!(registered);
            diagnostics["configuration_loaded"] = serde_json::json!(configuration_loaded);
            diagnostics["managed_root"] = serde_json::json!(cfg.root);
            println!("{}", serde_json::to_string_pretty(&diagnostics)?);
        }
        Action::OtaInit(args) => {
            let cfg = Config::load(&args.common.config)?;
            emissary::ota::initialize(
                &args.common.config,
                &cfg.root.join("ota"),
                &std::env::current_exe()?,
                emissary::VERSION,
                args.public_keys,
                cfg.allow_http_uploads,
            )?;
        }
        Action::Service(args) => emissary::service::manage(
            &args.action,
            &std::path::absolute(args.common.config)?,
            args.user.as_deref(),
        )?,
        Action::ServiceRun(args) => {
            #[cfg(windows)]
            return emissary::service::dispatch(std::path::absolute(args.config)?);
            #[cfg(not(windows))]
            {
                let cfg = Config::load(&args.config)?;
                return emissary::run_configured(&args.config, cfg, cancel).await;
            }
        }
        Action::Run(args) => {
            let cfg = Config::load(&args.config)?;
            if std::env::var_os("ARTIFACTORY_OTA_DIRECTORY").is_some() {
                let eof = cancel.clone();
                tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut tokio::io::sink()).await;
                    eof.cancel();
                });
            }
            return emissary::run_configured(&args.config, cfg, cancel).await;
        }
    }
    Ok(())
}
