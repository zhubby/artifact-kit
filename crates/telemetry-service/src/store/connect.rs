use super::{BusinessAuthorizationRepository, TelemetryStore};
use crate::Service;
use anyhow::Result;
use artifactory_database::{PoolSettings, connect as database, with_options};
use artifactory_runtime_protocol::service;
use std::{env, time::Duration};
pub async fn connect() -> Result<Service> {
    let write = database(
        &service::required("DATABASE_URL")?,
        super::models(),
        PoolSettings::new(12).timeout(Duration::from_secs(5)),
    )
    .await?;
    artifactory_database::require_timescale(&write).await?;
    artifactory_database::migrate(&write, &super::MIGRATIONS).await?;
    let read_options = with_options(
        &env::var("ARTIFACTORY_TELEMETRY_READ_DATABASE_URL")
            .unwrap_or(service::required("DATABASE_URL")?),
        &[
            ("default_transaction_read_only", "on"),
            ("statement_timeout", "5000"),
        ],
    )?;
    let read = database(
        &read_options,
        super::models(),
        PoolSettings::new(4).timeout(Duration::from_secs(3)),
    )
    .await?;
    artifactory_database::validate(&read, &super::MIGRATIONS).await?;
    let business_options = with_options(
        &service::required("ARTIFACTORY_BUSINESS_DATABASE_URL")?,
        &[
            ("default_transaction_read_only", "on"),
            ("statement_timeout", "3000"),
        ],
    )?;
    let business = database(
        &business_options,
        Default::default(),
        PoolSettings::new(4).timeout(Duration::from_secs(3)),
    )
    .await?;
    artifactory_database::require_timescale(&business).await?;
    artifactory_database::validate_versions(&business, &[(2026111301, "0000_baseline.sql")])
        .await?;
    Ok(Service {
        store: TelemetryStore::new(write, read),
        authorization: BusinessAuthorizationRepository::new(business),
    })
}
