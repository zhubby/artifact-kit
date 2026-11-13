//! Notification connections share pool transport policy and have an owned lifecycle.
use artifactory_database::{PoolSettings, connect, listen, params, sql, with_options};
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
async fn listener_rejects_invalid_channels_without_connecting() {
    for channel in ["", "work;SELECT 1", "work\"", "a b", &"a".repeat(64)] {
        assert!(
            listen("postgres://localhost:1/test", channel)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn notifications_follow_commit_and_reconnect_after_connection_loss() {
    let url = std::env::var("ARTIFACTORY_TEST_DATABASE_URL").expect("isolated database URL");
    let application = format!("listen_{}", Uuid::new_v4().simple());
    let scoped = with_options(&url, &[("application_name", &application)]).unwrap();
    let mut db = connect(&url, Default::default(), PoolSettings::new(1))
        .await
        .unwrap();
    let mut listener = listen(&scoped, &application).await.unwrap();
    let mut tx = db.transaction().await.unwrap();
    sql::execute(
        &mut tx,
        "SELECT pg_notify($1,'rolled back')",
        params![application],
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.recv())
            .await
            .is_err()
    );
    let mut tx = db.transaction().await.unwrap();
    sql::execute(
        &mut tx,
        "SELECT pg_notify($1,'committed')",
        params![application],
    )
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.recv())
            .await
            .is_err()
    );
    tx.commit().await.unwrap();
    let message = tokio::time::timeout(Duration::from_secs(5), listener.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.channel(), application);
    assert_eq!(message.payload(), "committed");
    let terminated: bool = sql::scalar(
        &mut db,
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name=$1",
        params![application],
    )
    .await
    .unwrap();
    assert!(terminated);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), listener.recv())
            .await
            .unwrap()
            .is_err()
    );
    drop(listener);
    let mut reconnected = listen(&scoped, &application).await.unwrap();
    sql::execute(
        &mut db,
        "SELECT pg_notify($1,'reconnected')",
        params![application],
    )
    .await
    .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), reconnected.recv())
            .await
            .unwrap()
            .unwrap()
            .payload(),
        "reconnected"
    );
    drop(reconnected);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let closed: bool = sql::scalar(
                &mut db,
                "SELECT NOT EXISTS(SELECT 1 FROM pg_stat_activity WHERE application_name=$1)",
                params![application],
            )
            .await
            .unwrap();
            if closed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropping a listener must close its dedicated backend");
}
