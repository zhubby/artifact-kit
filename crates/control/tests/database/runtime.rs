use super::*;
use artifactory_database::{params, sql};
use toasty::stmt::Type;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn source_public_address_does_not_replace_internal_rpc_and_old_snapshots_still_work() {
    use artifactory_control::runtime_bridge;
    let db = Database::new().await;
    let (project, _, _) = db.fixture().await;
    let repository = Uuid::new_v4();
    let snapshot = Uuid::new_v4();
    sql::execute(&mut db.pool.clone(), "INSERT INTO source_repositories(id,project_id,repository_url,cache_key) VALUES($1,$2,'https://example.com/repo.git',$3)", params![repository, project, crypto::hash(repository.to_string())]).await.unwrap();
    sql::execute(&mut db.pool.clone(), "INSERT INTO source_snapshots(id,project_id,root_repository_id,commit_sha,manifest,digest,service_origin,public_origin) VALUES($1,$2,$3,$4,'{}',$5,'https://source.internal:8090','https://source.example.com/shard-0')", params![snapshot, project, repository, "0".repeat(40), "a".repeat(64)]).await.unwrap();
    assert_eq!(
        runtime_bridge::source_origin(&db.state, snapshot)
            .await
            .unwrap()
            .as_deref(),
        Some("https://source.internal:8090")
    );
    assert_eq!(
        runtime_bridge::source_download_origin(&db.state, snapshot)
            .await
            .unwrap()
            .as_deref(),
        Some("https://source.example.com/shard-0")
    );
    sql::execute(
        &mut db.pool.clone(),
        "UPDATE source_snapshots SET public_origin=NULL WHERE id=$1",
        params![snapshot],
    )
    .await
    .unwrap();
    assert_eq!(
        runtime_bridge::source_download_origin(&db.state, snapshot)
            .await
            .unwrap()
            .as_deref(),
        Some("https://source.internal:8090")
    );
    sql::execute(
        &mut db.pool.clone(),
        "UPDATE source_snapshots SET service_origin=NULL WHERE id=$1",
        params![snapshot],
    )
    .await
    .unwrap();
    assert!(
        runtime_bridge::source_download_origin(&db.state, snapshot)
            .await
            .unwrap()
            .is_none()
    );
    db.close().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn native_run_outbox_is_atomic_and_cancel_is_a_new_version() {
    let db = Database::new().await;
    let (project, build, _) = db.fixture().await;
    sql::execute(
        &mut db.pool.clone(),
        "UPDATE builds SET planner_backend='kubernetes',status='planning' WHERE id=$1",
        params![build],
    )
    .await
    .unwrap();
    let run = Uuid::new_v4();
    let mut tx_db = db.pool.clone();
    let mut tx = tx_db.transaction().await.unwrap();
    sql::execute(
        &mut tx,
        "INSERT INTO planning_runs(id,build_id,purpose,model_config) VALUES($1,$2,'initial','{}')",
        params![run, build],
    )
    .await
    .unwrap();
    assert_eq!(
        sql::scalar::<i64>(
            &mut tx,
            "SELECT count(*) FROM runtime_intents WHERE run_id=$1",
            params![run]
        )
        .await
        .unwrap(),
        1
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        sql::scalar::<i64>(
            &mut db.pool.clone(),
            "SELECT count(*) FROM runtime_intents WHERE run_id=$1",
            params![run]
        )
        .await
        .unwrap(),
        0
    );
    sql::execute(
        &mut db.pool.clone(),
        "INSERT INTO planning_runs(id,build_id,purpose,model_config) VALUES($1,$2,'initial','{}')",
        params![run, build],
    )
    .await
    .unwrap();
    sql::execute(
        &mut db.pool.clone(),
        "UPDATE planning_runs SET state='canceled' WHERE id=$1",
        params![run],
    )
    .await
    .unwrap();
    let row=sql::record(&mut db.pool.clone(), "SELECT authority,(SELECT count(*) FROM runtime_intents WHERE run_id=$1) AS events FROM planning_runs WHERE id=$1", params![run], &[("authority", Type::String),("events", Type::Bool)]).await.unwrap();
    assert_eq!(
        row.decode::<String>("authority")
            .expect("database projection"),
        "kubernetes"
    );
    assert_eq!(row.decode::<i64>("events").expect("database projection"), 2);
    // Changing project default leaves the existing Build pinned to its original backend.
    sql::execute(
        &mut db.pool.clone(),
        "UPDATE projects SET planner_backend='poll' WHERE id=$1",
        params![project],
    )
    .await
    .unwrap();
    assert_eq!(
        sql::scalar::<String>(
            &mut db.pool.clone(),
            "SELECT planner_backend FROM builds WHERE id=$1",
            params![build]
        )
        .await
        .unwrap(),
        "kubernetes"
    );
    db.close().await;
}
