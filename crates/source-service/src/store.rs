//! Source database startup; table operations are shared in source-core::store.
use anyhow::Result;
use artifactory_source_core::store::SourceStore;
pub async fn connect(url: &str) -> Result<SourceStore> {
    let pool = artifactory_database::connect(
        url,
        artifactory_source_core::store::models(),
        artifactory_database::PoolSettings::new(24),
    )
    .await?;
    artifactory_database::require_timescale(&pool).await?;
    artifactory_database::migrate(&pool, &MIGRATIONS).await?;
    Ok(SourceStore::new(pool))
}

pub static MIGRATIONS: toasty::migration::MigrationSet = toasty::embed_migrations!("migrations");
