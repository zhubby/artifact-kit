//! PostgreSQL connection policy, migrations and value conversion shared by stores.
mod migrations;
pub mod sql;
pub use migrations::{migrate, validate, validate_versions};
use std::time::Duration;
use toasty::{Db, schema::app::ModelSet};
pub use toasty_core::driver::operation::IsolationLevel;

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Backend(#[from] toasty::Error),
    #[error("resource not found")]
    NotFound,
    #[error("database value conversion failed: {0}")]
    Decode(&'static str),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Constraint {
    Unique,
    ForeignKey,
    Check,
    NotNull,
}
/// Read PostgreSQL SQLSTATE from the typed driver cause, never from message text.
pub fn constraint(error: &(dyn std::error::Error + 'static)) -> Option<Constraint> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(error) = error.downcast_ref::<tokio_postgres::Error>()
            && let Some(error) = error.as_db_error()
        {
            return match error.code().code() {
                "23505" => Some(Constraint::Unique),
                "23503" => Some(Constraint::ForeignKey),
                "23514" => Some(Constraint::Check),
                "23502" => Some(Constraint::NotNull),
                _ => None,
            };
        }
        current = error.source();
    }
    None
}
impl Error {
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound)
            || matches!(self,Self::Backend(error) if error.is_record_not_found())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PoolSettings {
    pub capacity: usize,
    pub acquire_timeout: Duration,
}
impl PoolSettings {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            acquire_timeout: Duration::from_secs(30),
        }
    }
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.acquire_timeout = timeout;
        self
    }
}
/// Append server startup options without dropping TLS or caller-supplied options.
pub fn with_options(url: &str, options: &[(&str, &str)]) -> anyhow::Result<String> {
    let mut parsed = url::Url::parse(url)?;
    let pairs = parsed
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect::<Vec<_>>();
    let mut startup = String::new();
    let mut retained = Vec::new();
    for (key, value) in pairs {
        if key == "options" {
            startup.push_str(&value);
            startup.push(' ');
        } else {
            retained.push((key, value));
        }
    }
    for (key, value) in options {
        anyhow::ensure!(
            key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "invalid PostgreSQL option name"
        );
        anyhow::ensure!(
            value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_,.-".contains(c)),
            "invalid PostgreSQL option value"
        );
        startup.push_str(&format!("-c {key}={value} "));
    }
    parsed.set_query(None);
    parsed
        .query_pairs_mut()
        .extend_pairs(retained)
        .append_pair("options", startup.trim_end());
    Ok(parsed.into())
}
/// Select an isolated service database while retaining transport settings.
pub fn with_database(url: &str, name: &str) -> anyhow::Result<String> {
    let mut parsed = url::Url::parse(url)?;
    parsed.set_path(name);
    Ok(parsed.into())
}
pub async fn connect(url: &str, models: ModelSet, settings: PoolSettings) -> Result<Db> {
    Ok(Db::builder()
        .models(models)
        .max_pool_size(settings.capacity)
        .pool_wait_timeout(Some(settings.acquire_timeout))
        .pool_create_timeout(Some(settings.acquire_timeout))
        .pool_max_connection_lifetime(Some(Duration::from_secs(30 * 60)))
        .pool_max_connection_idle_time(Some(Duration::from_secs(10 * 60)))
        .connect(url)
        .await?)
}
/// Require administrator-provisioned TimescaleDB without privileged server settings.
pub async fn require_timescale(db: &Db) -> anyhow::Result<()> {
    let version: Option<String> = sql::optional_scalar(
        &mut db.clone(),
        "SELECT extversion FROM pg_extension WHERE extname='timescaledb'",
        params![],
    )
    .await?;
    let version=version.ok_or_else(||anyhow::anyhow!("TimescaleDB is required: provision this database with CREATE EXTENSION timescaledb as its administrator"))?;
    let mut parts = version
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok());
    anyhow::ensure!(
        (parts.next().unwrap_or(0), parts.next().unwrap_or(0)) >= (2, 32),
        "TimescaleDB 2.32 or newer is required; found {version}"
    );
    sql::execute(
        &mut db.clone(),
        "SELECT time_bucket(INTERVAL '1 minute', CURRENT_TIMESTAMP)",
        params![],
    )
    .await?;
    Ok(())
}
