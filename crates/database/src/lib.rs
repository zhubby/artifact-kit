//! Database prerequisites shared by Artifactory backend services.

/// Require an administrator-provisioned TimescaleDB extension before starting a service.
pub async fn require_timescale(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    let version: Option<String> =
        sqlx::query_scalar("SELECT extversion FROM pg_extension WHERE extname='timescaledb'")
            .fetch_optional(pool)
            .await?;
    let version = version.ok_or_else(|| anyhow::anyhow!(
        "TimescaleDB is required: provision this database with CREATE EXTENSION timescaledb as its administrator"
    ))?;
    let mut parts = version
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok());
    anyhow::ensure!(
        (parts.next().unwrap_or(0), parts.next().unwrap_or(0)) >= (2, 32),
        "TimescaleDB 2.32 or newer is required; found {version}"
    );
    // Calling a public function loads the extension and verifies it is usable without
    // granting application roles access to administrator-only server settings.
    sqlx::query("SELECT time_bucket(INTERVAL '1 minute', CURRENT_TIMESTAMP)")
        .execute(pool)
        .await?;
    Ok(())
}
