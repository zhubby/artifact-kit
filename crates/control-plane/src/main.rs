use anyhow::{Context, Result, ensure};
use artifactory_control_plane::{
    api, auth,
    grpc::EmissaryService,
    planning::PlanningService,
    protocol::{
        control_server::ControlServer, enrollment_server::EnrollmentServer,
        planner_enrollment_server::PlannerEnrollmentServer, planning_server::PlanningServer,
    },
    scheduler,
    settings::Settings,
    state::AppState,
    storage::Storage,
};
use clap::{Parser, Subcommand};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

#[derive(Parser)]
#[command(version)]
struct Arguments {
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    Serve,
    Migrate,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "artifactory_control_plane=info,tower_http=info".into()),
        )
        .init();
    let arguments = Arguments::parse();
    let settings = Settings::from_env()?;
    tokio::fs::create_dir_all(&settings.data_dir).await?;
    let store =
        artifactory_control_plane::store::ControlStore::connect(&settings.database_url).await?;
    if matches!(arguments.command, Some(Command::Migrate)) {
        return Ok(());
    }
    if store.users_empty().await? {
        let username =
            std::env::var("ARTIFACTORY_BOOTSTRAP_USERNAME").unwrap_or_else(|_| "admin".into());
        let password = std::env::var("ARTIFACTORY_BOOTSTRAP_PASSWORD")
            .context("ARTIFACTORY_BOOTSTRAP_PASSWORD is required for the first administrator")?;
        ensure!(
            !username.is_empty() && username.len() <= 100,
            "invalid bootstrap username"
        );
        let hash = auth::password_hash(&password)?;
        store.bootstrap_admin(username, hash).await?;
    }
    let storage = Storage::new(&settings)?;
    storage.initialize().await?;
    let state = AppState {
        store,
        settings: Arc::new(settings.clone()),
        storage: Arc::new(storage),
        connections: Arc::new(RwLock::new(HashMap::new())),
    };
    scheduler::start(state.clone());
    let server_identity = Identity::from_pem(
        tokio::fs::read(settings.pki_dir.join("server.pem")).await?,
        tokio::fs::read(settings.pki_dir.join("server.key")).await?,
    );
    let ca = Certificate::from_pem(tokio::fs::read(settings.pki_dir.join("ca.pem")).await?);
    let tls = ServerTlsConfig::new()
        .identity(server_identity)
        .client_ca_root(ca)
        .client_auth_optional(true);
    let emissary = EmissaryService(state.clone());
    let grpc = Server::builder()
        .tls_config(tls)?
        .add_service(EnrollmentServer::new(emissary.clone()).max_decoding_message_size(64 * 1024))
        .add_service(ControlServer::new(emissary).max_decoding_message_size(128 * 1024))
        .add_service(
            PlannerEnrollmentServer::new(PlanningService(state.clone()))
                .max_decoding_message_size(32 * 1024),
        )
        .add_service(
            PlanningServer::new(PlanningService(state.clone()))
                .max_decoding_message_size(768 * 1024)
                .max_encoding_message_size(2 * 1024 * 1024),
        )
        .serve_with_shutdown(settings.grpc_bind.parse()?, shutdown());
    let listener = tokio::net::TcpListener::bind(&settings.http_bind).await?;
    tracing::info!(http=%settings.http_bind,grpc=%settings.grpc_bind,"artifactory control plane ready");
    let http = axum::serve(listener, api::router(state)).with_graceful_shutdown(shutdown());
    tokio::try_join!(async { http.await.map_err(anyhow::Error::from) }, async {
        grpc.await.map_err(anyhow::Error::from)
    })?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("signal handler");
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
