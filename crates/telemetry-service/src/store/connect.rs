use super::{BusinessAuthorizationRepository, TelemetryStore};
use crate::Service;
use anyhow::{Context, Result};
use artifactory_database::{PoolSettings, connect as database, with_options};
use artifactory_runtime_protocol::service;
use std::{env, time::Duration};
pub async fn connect() -> Result<Service> {
    let write = database(
        &service::required("DATABASE_URL")?,
        super::models(),
        PoolSettings::new(connection_budget(
            "ARTIFACTORY_TELEMETRY_WRITE_CONNECTIONS",
            12,
        )?)
        .timeout(Duration::from_secs(5)),
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
        PoolSettings::new(connection_budget(
            "ARTIFACTORY_TELEMETRY_READ_CONNECTIONS",
            4,
        )?)
        .timeout(Duration::from_secs(3)),
    )
    .await?;
    artifactory_database::validate(&read, &super::MIGRATIONS).await?;
    let authorization =
        business_authorization(&service::required("ARTIFACTORY_BUSINESS_DATABASE_URL")?).await?;
    Ok(Service {
        store: TelemetryStore::new(write, read),
        authorization,
    })
}

async fn business_authorization(url: &str) -> Result<BusinessAuthorizationRepository> {
    let business_options = with_options(
        url,
        &[
            ("default_transaction_read_only", "on"),
            ("statement_timeout", "3000"),
        ],
    )?;
    let business = database(
        &business_options,
        Default::default(),
        PoolSettings::new(connection_budget(
            "ARTIFACTORY_TELEMETRY_AUTH_CONNECTIONS",
            4,
        )?)
        .timeout(Duration::from_secs(3)),
    )
    .await?;
    artifactory_database::require_timescale(&business).await?;
    // Authorization columns are unchanged by HA. Accept both known histories,
    // including the baseline during an ordered Control/Telemetry upgrade.
    if artifactory_database::validate_versions(
        &business,
        &[
            (2026111301, "0000_baseline.sql"),
            (2026111401, "0001_control_replication.sql"),
        ],
    )
    .await
    .is_err()
    {
        artifactory_database::validate_versions(&business, &[(2026111301, "0000_baseline.sql")])
            .await
            .context("validate business database version: the authorization role needs SELECT on __toasty_migrations (see docs/telemetry.md)")?;
    }
    Ok(BusinessAuthorizationRepository::new(business))
}

#[cfg(test)]
mod tests;

fn connection_budget(name: &str, default: usize) -> Result<usize> {
    let count = env::var(name)
        .ok()
        .map(|v| v.parse::<usize>())
        .transpose()?
        .unwrap_or(default);
    anyhow::ensure!(
        (2..=128).contains(&count),
        "{name} must be between 2 and 128"
    );
    Ok(count)
}
