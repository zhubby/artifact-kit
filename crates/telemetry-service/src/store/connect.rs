use super::{BusinessAuthorizationRepository, TelemetryStore};
use crate::Service;
use anyhow::Result;
use artifactory_runtime_protocol::service;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::{env, time::Duration};
pub async fn connect() -> Result<Service> {
    let write = PgPoolOptions::new()
        .max_connections(12)
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
        .max_connections(4)
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
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(3))
        .connect_with(business_options)
        .await?;
    artifactory_database::require_timescale(&business).await?;
    Ok(Service {
        store: TelemetryStore::new(write, read),
        authorization: BusinessAuthorizationRepository::new(business),
    })
}
