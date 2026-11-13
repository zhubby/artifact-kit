//! Source database startup; table operations are shared in source-core::store.
use anyhow::Result;
use artifactory_source_core::store::SourceStore;
pub async fn connect(url: &str) -> Result<SourceStore> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(24)
        .connect(url)
        .await?;
    artifactory_database::require_timescale(&pool).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(SourceStore::new(pool))
}
