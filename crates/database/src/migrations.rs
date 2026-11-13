//! Serialize embedded baselines across all service processes on a database.
use crate::{PoolSettings, connect, params, sql};
use anyhow::{Result, ensure};
use toasty::{Db, migration::MigrationSet};

async fn applied(db: &Db) -> Result<Option<Vec<(i64, String)>>> {
    let present:bool=sql::scalar(&mut db.clone(),"SELECT EXISTS(SELECT 1 FROM pg_class WHERE relname='__toasty_migrations' AND relnamespace=current_schema()::regnamespace)",params![]).await?;
    if !present {
        return Ok(None);
    }
    Ok(Some(
        sql::tuples(
            &mut db.clone(),
            "SELECT id,name FROM __toasty_migrations ORDER BY id",
            params![],
        )
        .await?,
    ))
}
async fn unrecognized_objects(db: &Db) -> Result<bool> {
    Ok(sql::scalar(&mut db.clone(),
        "SELECT EXISTS(SELECT 1 FROM pg_class c WHERE c.relnamespace=current_schema()::regnamespace AND c.relkind IN ('r','p','v','m','S','f') AND c.relname<>'__toasty_migrations' AND NOT EXISTS(SELECT 1 FROM pg_depend d WHERE d.classid='pg_class'::regclass AND d.objid=c.oid AND d.deptype='e') UNION ALL SELECT 1 FROM pg_proc p WHERE p.pronamespace=current_schema()::regnamespace AND NOT EXISTS(SELECT 1 FROM pg_depend d WHERE d.classid='pg_proc'::regclass AND d.objid=p.oid AND d.deptype='e') UNION ALL SELECT 1 FROM pg_type t WHERE t.typnamespace=current_schema()::regnamespace AND t.typtype IN ('e','d') AND NOT EXISTS(SELECT 1 FROM pg_depend d WHERE d.classid='pg_type'::regclass AND d.objid=t.oid AND d.deptype='e'))",
        params![]).await?)
}
fn verify_ids(ids: &[(i64, String)], set: &MigrationSet, complete: bool) -> Result<()> {
    for (id, name) in ids {
        ensure!(
            set.migrations()
                .iter()
                .any(|m| m.id() as i64 == *id && m.name() == name),
            "unrecognized database migration; restore the matching application or use a fresh database"
        );
    }
    if complete {
        ensure!(
            ids.len() == set.migrations().len(),
            "database baseline is missing; initialize it using a writable service connection"
        );
    }
    Ok(())
}
/// Check a read connection's version without writing migration metadata.
pub async fn validate(db: &Db, set: &MigrationSet) -> Result<()> {
    let ids = applied(db).await?.ok_or_else(|| {
        anyhow::anyhow!("unrecognized database schema; a fresh Toasty baseline is required")
    })?;
    verify_ids(&ids, set, true)
}
/// Validate another service's schema without importing its table models.
pub async fn validate_versions(db: &Db, expected: &[(i64, &str)]) -> Result<()> {
    let ids = applied(db).await?.ok_or_else(|| {
        anyhow::anyhow!("unrecognized database schema; a fresh Toasty baseline is required")
    })?;
    ensure!(
        ids.len() == expected.len()
            && ids.iter().all(|(id, name)| expected
                .iter()
                .any(|(wanted, label)| id == wanted && name == label)),
        "unrecognized database migration version"
    );
    Ok(())
}
/// The lock lives on a separate connection so even a one-slot application pool can migrate.
pub async fn migrate(db: &Db, set: &MigrationSet) -> Result<()> {
    let readonly: bool = sql::scalar(
        &mut db.clone(),
        "SELECT current_setting('transaction_read_only')='on'",
        params![],
    )
    .await?;
    ensure!(!readonly, "read-only connections cannot run migrations");
    let mut lock_db = connect(&db.driver().url(), Default::default(), PoolSettings::new(1)).await?;
    let mut lock = lock_db.transaction().await?;
    sql::execute(
        &mut lock,
        "SELECT pg_advisory_xact_lock(438284731022931)",
        params![],
    )
    .await?;
    match applied(db).await? {
        Some(ids) if !ids.is_empty() => verify_ids(&ids, set, false)?,
        _ => ensure!(
            !unrecognized_objects(db).await?,
            "unrecognized existing schema; create a new database instead of migrating or clearing old data"
        ),
    }
    set.apply(db).await?;
    validate(db, set).await?;
    lock.commit().await?;
    Ok(())
}
