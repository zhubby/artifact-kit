//! Native result streams retain their connection and close it when abandoned.
use artifactory_database::{PoolSettings, connect, params, sql, with_options};
use std::time::Duration;
use uuid::Uuid;

struct Fixture {
    admin: toasty::Db,
    db: toasty::Db,
    application: String,
}

impl Fixture {
    async fn new() -> Self {
        let url = std::env::var("ARTIFACTORY_TEST_DATABASE_URL").expect("isolated database URL");
        let application = format!("stream_{}", Uuid::new_v4().simple());
        let admin = connect(&url, Default::default(), PoolSettings::new(1))
            .await
            .unwrap();
        let db = connect(
            &with_options(&url, &[("application_name", &application)]).unwrap(),
            Default::default(),
            PoolSettings::new(1),
        )
        .await
        .unwrap();
        Self {
            admin,
            db,
            application,
        }
    }

    async fn pid(&self) -> i32 {
        sql::scalar(&mut self.db.clone(), "SELECT pg_backend_pid()", params![])
            .await
            .unwrap()
    }

    async fn wait_for(&self, condition: &str) {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let ready: bool = sql::scalar(
                    &mut self.admin.clone(),
                    condition,
                    params![self.application],
                )
                .await
                .unwrap();
                if ready {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out observing {condition}"));
    }

    async fn wait_closed(&self) {
        self.wait_for(
            "SELECT NOT EXISTS(SELECT 1 FROM pg_stat_activity WHERE application_name=$1)",
        )
        .await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn native_stream_pages_reuse_connection_only_after_eof() {
    let fixture = Fixture::new().await;
    let pid = fixture.pid().await;
    let mut pages = sql::scalar_pages::<i64>(
        &fixture.db,
        "SELECT i::bigint FROM generate_series(1,256) i",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(
        pages.next_page().await.unwrap(),
        (1..=128).collect::<Vec<_>>()
    );
    // With a one-slot pool, an unfinished stream must retain the lease.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), fixture.db.connection())
            .await
            .is_err()
    );
    assert_eq!(
        pages.next_page().await.unwrap(),
        (129..=256).collect::<Vec<_>>()
    );
    assert!(pages.next_page().await.unwrap().is_empty());
    assert!(pages.next_page().await.unwrap().is_empty());
    assert_eq!(fixture.pid().await, pid);
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn abandoning_native_stream_closes_socket_instead_of_draining() {
    let fixture = Fixture::new().await;
    let pid = fixture.pid().await;
    let mut pages = sql::scalar_pages::<i64>(
        &fixture.db,
        "SELECT i::bigint FROM generate_series(1,10000000) i",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(pages.next_page().await.unwrap().len(), 128);
    drop(pages);
    fixture.wait_closed().await;
    assert_ne!(fixture.pid().await, pid);
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn cancelling_blocked_native_stream_closes_connection_and_releases_lock_wait() {
    let fixture = Fixture::new().await;
    let mut admin = fixture.admin.clone();
    let mut lock = admin.transaction().await.unwrap();
    let key = i64::from(Uuid::new_v4().as_u128() as i32);
    sql::execute(&mut lock, "SELECT pg_advisory_xact_lock($1)", params![key])
        .await
        .unwrap();
    let db = fixture.db.clone();
    let pending = tokio::spawn(async move {
        let mut pages = sql::scalar_pages::<bool>(
            &db,
            "SELECT pg_advisory_xact_lock($1) IS NULL",
            params![key],
        )
        .await
        .unwrap();
        pages.next_page().await.unwrap()
    });
    // Use a separate observer because admin's only slot owns the blocking lock.
    let url = std::env::var("ARTIFACTORY_TEST_DATABASE_URL").unwrap();
    let observer = Fixture {
        admin: connect(&url, Default::default(), PoolSettings::new(1))
            .await
            .unwrap(),
        db: fixture.db.clone(),
        application: fixture.application.clone(),
    };
    observer.wait_for("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE application_name=$1 AND wait_event='advisory')").await;
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    observer.wait_closed().await;
    lock.rollback().await.unwrap();
    assert!(fixture.pid().await > 0);
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn native_stream_propagates_error_after_first_page_and_pool_recovers() {
    let fixture = Fixture::new().await;
    let mut pages = sql::scalar_pages::<i32>(
        &fixture.db,
        "SELECT 1/(129-i) FROM generate_series(1,256) i",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(pages.next_page().await.unwrap().len(), 128);
    assert!(pages.next_page().await.is_err());
    drop(pages);
    fixture.wait_closed().await;
    assert!(fixture.pid().await > 0);
}
