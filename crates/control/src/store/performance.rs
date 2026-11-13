//! Same fixture and timing boundary for both database backends.
use super::{ControlStore, ExecutionEvent};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::time::Instant;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL; reports critical store p95"]
async fn critical_store_latency() {
    let url = std::env::var("ARTIFACTORY_TEST_DATABASE_URL").unwrap();
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let schema = format!("perf_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let options: PgConnectOptions = url.parse().unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let store = ControlStore::new(pool.clone());
    let project = Uuid::new_v4();
    let build = Uuid::new_v4();
    let node = Uuid::new_v4();
    let session = Uuid::new_v4();
    let target = json!({"os":"linux","arch":"amd64","steps":[{"name":"compile","run":"true"}],"artifacts":[]});
    sqlx::query("INSERT INTO projects(id,name,repository_url) VALUES($1,'perf','https://example.invalid/repo.git')").bind(project).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO builds(id,project_id,requested_ref,status,source_repository_url) VALUES($1,$2,'main','queued','https://example.invalid/repo.git')",
    )
    .bind(build)
    .bind(project)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO nodes(id,name,os,arch,pool_id,capabilities,certificate_hash,certificate_expires_at,active_session,last_seen) VALUES($1,'perf','linux','amd64','00000000-0000-0000-0000-000000000001','[\"git\",\"bash\"]',$2,now()+interval '1 day',$3,now())").bind(node).bind(node.to_string()).bind(session).execute(&pool).await.unwrap();
    let mut claim = Vec::new();
    let mut heartbeat = Vec::new();
    let mut event = Vec::new();
    for index in 0..288 {
        let job = Uuid::new_v4();
        sqlx::query("INSERT INTO jobs(id,build_id,target,target_config) VALUES($1,$2,$3,$4)")
            .bind(job)
            .bind(build)
            .bind(index.to_string())
            .bind(&target)
            .execute(&pool)
            .await
            .unwrap();
        let started = Instant::now();
        let leased = store.claim_job(node, session).await.unwrap().unwrap();
        let claim_us = started.elapsed().as_micros() as u64;
        let started = Instant::now();
        store
            .record_heartbeat(node, session, json!(["git", "bash"]), json!({}))
            .await
            .unwrap();
        assert!(
            store
                .renew_attempt(node, session, leased.attempt, &leased.lease_token)
                .await
                .unwrap()
        );
        let heartbeat_us = started.elapsed().as_micros() as u64;
        store
            .submit_execution_event(
                node,
                session,
                leased.attempt,
                ExecutionEvent {
                    kind: "accept",
                    sequence: 0,
                    lease_token: &leased.lease_token,
                    payload_json: "{}",
                },
            )
            .await
            .unwrap();
        let started = Instant::now();
        store
            .submit_execution_event(
                node,
                session,
                leased.attempt,
                ExecutionEvent {
                    kind: "step_started",
                    sequence: 1,
                    lease_token: &leased.lease_token,
                    payload_json: "{\"index\":0}",
                },
            )
            .await
            .unwrap();
        let event_us = started.elapsed().as_micros() as u64;
        sqlx::query("UPDATE attempts SET status='failed' WHERE id=$1")
            .bind(leased.attempt)
            .execute(&pool)
            .await
            .unwrap();
        if index >= 32 {
            claim.push(claim_us);
            heartbeat.push(heartbeat_us);
            event.push(event_us);
        }
    }
    let p95 = |mut values: Vec<u64>| {
        values.sort_unstable();
        values[(values.len() * 95).div_ceil(100) - 1]
    };
    println!(
        "PERF {}",
        json!({"scheduling_us":p95(claim),"heartbeat_us":p95(heartbeat),"event_us":p95(event),"samples":256})
    );
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
}
