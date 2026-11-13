use anyhow::{Result, ensure};
use artifactory_emissary_protocol::telemetry_server::TelemetryServer;
use artifactory_runtime_protocol::service;
use artifactory_telemetry_service::{Service, query};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::{env, path::PathBuf, time::Duration};
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let write = PgPoolOptions::new()
        .max_connections(connection_budget(
            "ARTIFACTORY_TELEMETRY_WRITE_CONNECTIONS",
            12,
        )?)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&service::required("DATABASE_URL")?)
        .await?;
    artifactory_database::require_timescale(&write).await?;
    sqlx::migrate!("./migrations").run(&write).await?;
    let read_options = env::var("ARTIFACTORY_TELEMETRY_READ_DATABASE_URL")
        .unwrap_or(service::required("DATABASE_URL")?)
        .parse::<PgConnectOptions>()?
        .options([
            ("default_transaction_read_only", "on"),
            ("statement_timeout", "5000"),
        ]);
    let read = PgPoolOptions::new()
        .max_connections(connection_budget(
            "ARTIFACTORY_TELEMETRY_READ_CONNECTIONS",
            4,
        )?)
        .acquire_timeout(Duration::from_secs(3))
        .connect_with(read_options)
        .await?;
    let business_options = service::required("ARTIFACTORY_BUSINESS_DATABASE_URL")?
        .parse::<PgConnectOptions>()?
        .options([
            ("default_transaction_read_only", "on"),
            ("statement_timeout", "3000"),
        ]);
    let business = PgPoolOptions::new()
        .max_connections(connection_budget(
            "ARTIFACTORY_TELEMETRY_AUTH_CONNECTIONS",
            4,
        )?)
        .acquire_timeout(Duration::from_secs(3))
        .connect_with(business_options)
        .await?;
    artifactory_database::require_timescale(&business).await?;
    let token = service::required("ARTIFACTORY_TELEMETRY_TOKEN")?;
    ensure!(
        token.len() >= 32,
        "telemetry service token must contain at least 32 characters"
    );
    let state = Service {
        write: write.clone(),
        read,
        business,
    };
    let pki = PathBuf::from(service::required("ARTIFACTORY_PKI_DIR")?);
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(
            tokio::fs::read(pki.join("server.pem")).await?,
            tokio::fs::read(pki.join("server.key")).await?,
        ))
        .client_ca_root(Certificate::from_pem(
            tokio::fs::read(pki.join("ca.pem")).await?,
        ));
    let grpc = Server::builder()
        .timeout(Duration::from_secs(15))
        .concurrency_limit_per_connection(2)
        .tls_config(tls)?
        .add_service(
            TelemetryServer::new(state.clone())
                .max_decoding_message_size(artifactory_telemetry_model::MAX_BATCH_BYTES + 1024),
        )
        .serve(
            env::var("ARTIFACTORY_TELEMETRY_GRPC_BIND")
                .unwrap_or_else(|_| "127.0.0.1:8444".into())
                .parse()?,
        );
    let http_bind =
        env::var("ARTIFACTORY_TELEMETRY_HTTP_BIND").unwrap_or_else(|_| "127.0.0.1:8093".into());
    // Ledger retention exceeds the 48h replay window. Inventory bodies are stored once per digest.
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            if let Err(error) = retain(&write).await {
                tracing::warn!(%error,"telemetry retention maintenance failed");
            }
        }
    });
    tracing::info!("telemetry service ready");
    tokio::select! {r=grpc=>r?,r=service::serve(&http_bind,query::router(state,token))=>r?,_=shutdown()=>{}}
    Ok(())
}

fn connection_budget(name: &str, default: u32) -> Result<u32> {
    let value = env::var(name).ok();
    parse_budget(name, value.as_deref(), default)
}

fn parse_budget(name: &str, value: Option<&str>, default: u32) -> Result<u32> {
    let count = value.map(str::parse::<u32>).transpose()?.unwrap_or(default);
    ensure!(
        (2..=128).contains(&count),
        "{name} must be between 2 and 128"
    );
    Ok(count)
}

async fn retain(write: &sqlx::PgPool) -> Result<()> {
    let mut tx = write.begin().await?;
    let acquired: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtextextended('artifactory:telemetry-retention',12))",
    )
    .fetch_one(&mut *tx)
    .await?;
    if !acquired {
        return Ok(());
    }
    for statement in [
        "DELETE FROM telemetry_batches WHERE received_at<now()-interval '3 days'",
        "DELETE FROM inventory_observations WHERE sampled_at<now()-interval '180 days'",
        "DELETE FROM inventory_versions v WHERE NOT EXISTS(SELECT 1 FROM inventory_observations o WHERE o.node_id=v.node_id AND o.digest=v.digest) AND NOT EXISTS(SELECT 1 FROM latest_inventories l WHERE l.node_id=v.node_id AND l.digest=v.digest)",
    ] {
        sqlx::query(statement).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! { _=tokio::signal::ctrl_c()=>{}, _=terminate.recv()=>{} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::parse_budget;

    #[test]
    fn validates_connection_budgets() {
        assert_eq!(parse_budget("pool", None, 12).unwrap(), 12);
        assert_eq!(parse_budget("pool", Some("32"), 12).unwrap(), 32);
        for value in ["0", "1", "129", "-1", "abc"] {
            assert!(
                parse_budget("pool", Some(value), 12).is_err(),
                "accepted {value}"
            );
        }
    }
}
