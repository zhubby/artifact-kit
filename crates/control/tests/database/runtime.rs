use super::*;
use sqlx::Row;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn source_public_address_does_not_replace_internal_rpc_and_old_snapshots_still_work() {
    use artifactory_control::runtime_bridge;
    let db = Database::new().await;
    let (project, _, _) = db.fixture().await;
    let repository = Uuid::new_v4();
    let snapshot = Uuid::new_v4();
    sqlx::query("INSERT INTO source_repositories(id,project_id,repository_url,cache_key) VALUES($1,$2,'https://example.com/repo.git',$3)")
        .bind(repository).bind(project).bind(crypto::hash(repository.to_string())).execute(&db.state.pool).await.unwrap();
    sqlx::query("INSERT INTO source_snapshots(id,project_id,root_repository_id,commit_sha,manifest,digest,service_origin,public_origin) VALUES($1,$2,$3,$4,'{}',$5,'https://source.internal:8090','https://source.example.com/shard-0')")
        .bind(snapshot).bind(project).bind(repository).bind("0".repeat(40)).bind("a".repeat(64)).execute(&db.state.pool).await.unwrap();
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
    sqlx::query("UPDATE source_snapshots SET public_origin=NULL WHERE id=$1")
        .bind(snapshot)
        .execute(&db.state.pool)
        .await
        .unwrap();
    assert_eq!(
        runtime_bridge::source_download_origin(&db.state, snapshot)
            .await
            .unwrap()
            .as_deref(),
        Some("https://source.internal:8090")
    );
    sqlx::query("UPDATE source_snapshots SET service_origin=NULL WHERE id=$1")
        .bind(snapshot)
        .execute(&db.state.pool)
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
    sqlx::query("UPDATE builds SET planner_backend='kubernetes',status='planning' WHERE id=$1")
        .bind(build)
        .execute(&db.state.pool)
        .await
        .unwrap();
    let run = Uuid::new_v4();
    let mut tx = db.state.pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO planning_runs(id,build_id,purpose,model_config) VALUES($1,$2,'initial','{}')",
    )
    .bind(run)
    .bind(build)
    .execute(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM runtime_intents WHERE run_id=$1")
            .bind(run)
            .fetch_one(&mut *tx)
            .await
            .unwrap(),
        1
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM runtime_intents WHERE run_id=$1")
            .bind(run)
            .fetch_one(&db.state.pool)
            .await
            .unwrap(),
        0
    );
    sqlx::query(
        "INSERT INTO planning_runs(id,build_id,purpose,model_config) VALUES($1,$2,'initial','{}')",
    )
    .bind(run)
    .bind(build)
    .execute(&db.state.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE planning_runs SET state='canceled' WHERE id=$1")
        .bind(run)
        .execute(&db.state.pool)
        .await
        .unwrap();
    let row=sqlx::query("SELECT authority,(SELECT count(*) FROM runtime_intents WHERE run_id=$1) AS events FROM planning_runs WHERE id=$1").bind(run).fetch_one(&db.state.pool).await.unwrap();
    assert_eq!(row.get::<String, _>("authority"), "kubernetes");
    assert_eq!(row.get::<i64, _>("events"), 2);
    // Changing project default leaves the existing Build pinned to its original backend.
    sqlx::query("UPDATE projects SET planner_backend='poll' WHERE id=$1")
        .bind(project)
        .execute(&db.state.pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT planner_backend FROM builds WHERE id=$1")
            .bind(build)
            .fetch_one(&db.state.pool)
            .await
            .unwrap(),
        "kubernetes"
    );
    db.close().await;
}
