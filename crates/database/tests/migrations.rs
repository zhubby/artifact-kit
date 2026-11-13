//! Native migration and cancellation contracts against disposable schemas.
use artifactory_database::{PoolSettings, connect, migrate, params, sql, validate, with_options};
use toasty::migration::{MigrationFile, MigrationSet};
use uuid::Uuid;

static BASELINE: MigrationSet = MigrationSet::new(&[MigrationFile::new(
    1,
    "baseline",
    "CREATE TABLE counters(id integer PRIMARY KEY,value integer NOT NULL CHECK(value>=0)); INSERT INTO counters VALUES(1,0);",
)]);
static BROKEN: MigrationSet = MigrationSet::new(&[MigrationFile::new(
    1,
    "baseline",
    "CREATE TABLE partial(id integer); SELECT 1/0;",
)]);

struct Fixture {
    admin: toasty::Db,
    db: toasty::Db,
    url: String,
    schema: String,
}
impl Fixture {
    async fn new() -> Self {
        let url = std::env::var("ARTIFACTORY_TEST_DATABASE_URL").expect("isolated database URL");
        let admin = connect(&url, Default::default(), PoolSettings::new(1))
            .await
            .unwrap();
        let schema = format!("migration_{}", Uuid::new_v4().simple());
        sql::execute(
            &mut admin.clone(),
            &format!("CREATE SCHEMA {schema}"),
            params![],
        )
        .await
        .unwrap();
        let url = with_options(&url, &[("search_path", &schema)]).unwrap();
        let db = connect(&url, Default::default(), PoolSettings::new(1))
            .await
            .unwrap();
        Self {
            admin,
            db,
            url,
            schema,
        }
    }
    async fn clean(self) {
        sql::execute(
            &mut self.admin.clone(),
            &format!("DROP SCHEMA {} CASCADE", self.schema),
            params![],
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn concurrent_migrations_repeat_start_and_readonly_validation() {
    let fixture = Fixture::new().await;
    let other = connect(&fixture.url, Default::default(), PoolSettings::new(1))
        .await
        .unwrap();
    let (a, b) = tokio::join!(migrate(&fixture.db, &BASELINE), migrate(&other, &BASELINE));
    a.unwrap();
    b.unwrap();
    migrate(&fixture.db, &BASELINE).await.unwrap();
    assert_eq!(
        sql::scalar::<i64>(
            &mut fixture.db.clone(),
            "SELECT count(*) FROM __toasty_migrations",
            params![]
        )
        .await
        .unwrap(),
        1
    );
    let read = connect(
        &with_options(&fixture.url, &[("default_transaction_read_only", "on")]).unwrap(),
        Default::default(),
        PoolSettings::new(1),
    )
    .await
    .unwrap();
    validate(&read, &BASELINE).await.unwrap();
    assert!(migrate(&read, &BASELINE).await.is_err());
    assert!(
        sql::execute(&mut read.clone(), "UPDATE counters SET value=1", params![])
            .await
            .is_err()
    );
    fixture.clean().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn failed_baseline_rolls_back_and_can_restart() {
    let fixture = Fixture::new().await;
    assert!(migrate(&fixture.db, &BROKEN).await.is_err());
    assert!(
        !sql::scalar::<bool>(
            &mut fixture.db.clone(),
            "SELECT to_regclass('partial') IS NOT NULL",
            params![]
        )
        .await
        .unwrap()
    );
    migrate(&fixture.db, &BASELINE).await.unwrap();
    validate(&fixture.db, &BASELINE).await.unwrap();
    fixture.clean().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn unknown_schema_and_migration_versions_are_never_cleared() {
    let fixture = Fixture::new().await;
    sql::execute(
        &mut fixture.db.clone(),
        "CREATE TABLE old_data(value text)",
        params![],
    )
    .await
    .unwrap();
    sql::execute(
        &mut fixture.db.clone(),
        "INSERT INTO old_data VALUES('retained')",
        params![],
    )
    .await
    .unwrap();
    assert!(migrate(&fixture.db, &BASELINE).await.is_err());
    assert_eq!(
        sql::scalar::<String>(
            &mut fixture.db.clone(),
            "SELECT value FROM old_data",
            params![]
        )
        .await
        .unwrap(),
        "retained"
    );
    sql::execute(&mut fixture.db.clone(), "DROP TABLE old_data", params![])
        .await
        .unwrap();
    migrate(&fixture.db, &BASELINE).await.unwrap();
    sql::execute(
        &mut fixture.db.clone(),
        "UPDATE __toasty_migrations SET id=99",
        params![],
    )
    .await
    .unwrap();
    assert!(migrate(&fixture.db, &BASELINE).await.is_err());
    assert!(validate(&fixture.db, &BASELINE).await.is_err());
    fixture.clean().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn cancellation_and_savepoint_failures_rollback_without_leaking_locks() {
    let fixture = Fixture::new().await;
    migrate(&fixture.db, &BASELINE).await.unwrap();
    let mut db = fixture.db.clone();
    let mut tx = db.transaction().await.unwrap();
    sql::execute(&mut tx, "UPDATE counters SET value=1 WHERE id=1", params![])
        .await
        .unwrap();
    {
        let mut savepoint = tx.transaction().await.unwrap();
        assert!(
            sql::execute(
                &mut savepoint,
                "UPDATE counters SET value=-1 WHERE id=1",
                params![]
            )
            .await
            .is_err()
        );
        savepoint.rollback().await.unwrap();
    }
    tx.commit().await.unwrap();
    let (locked, ready) = tokio::sync::oneshot::channel();
    let mut db = fixture.db.clone();
    let task = tokio::spawn(async move {
        let mut tx = db.transaction().await.unwrap();
        sql::execute(&mut tx, "UPDATE counters SET value=2 WHERE id=1", params![])
            .await
            .unwrap();
        let _ = locked.send(());
        std::future::pending::<()>().await;
    });
    ready.await.unwrap();
    task.abort();
    let _ = task.await;
    let current = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sql::scalar::<i32>(
            &mut fixture.db.clone(),
            "SELECT value FROM counters WHERE id=1",
            params![],
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(current, 1);
    fixture.clean().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn lost_connection_rolls_back_and_pool_recovers() {
    let fixture = Fixture::new().await;
    migrate(&fixture.db, &BASELINE).await.unwrap();
    let mut db = fixture.db.clone();
    let mut tx = db.transaction().await.unwrap();
    let pid: i32 = sql::scalar(&mut tx, "SELECT pg_backend_pid()", params![])
        .await
        .unwrap();
    sql::execute(&mut tx, "UPDATE counters SET value=9 WHERE id=1", params![])
        .await
        .unwrap();
    assert!(
        sql::scalar::<bool>(
            &mut fixture.admin.clone(),
            "SELECT pg_terminate_backend($1)",
            params![pid]
        )
        .await
        .unwrap()
    );
    assert!(
        sql::execute(
            &mut tx,
            "UPDATE counters SET value=10 WHERE id=1",
            params![]
        )
        .await
        .is_err()
    );
    drop(tx);
    let current = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sql::scalar::<i32>(
            &mut fixture.db.clone(),
            "SELECT value FROM counters WHERE id=1",
            params![],
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(current, 0);
    fixture.clean().await;
}
