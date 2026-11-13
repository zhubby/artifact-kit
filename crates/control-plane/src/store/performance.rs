//! Same fixture and timing boundary for both database backends.
use super::{ControlStore, ExecutionEvent};
use artifactory_database::{PoolSettings, connect, with_options};
use artifactory_database::{params, sql};
use serde_json::json;
use std::time::Instant;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL; reports critical store p95"]
async fn critical_store_latency() {
    let url = std::env::var("ARTIFACTORY_TEST_DATABASE_URL").unwrap();
    let admin = connect(&url, Default::default(), PoolSettings::new(2))
        .await
        .unwrap();
    let schema = format!("perf_{}", Uuid::new_v4().simple());
    sql::execute(
        &mut admin.clone(),
        &format!("CREATE SCHEMA {schema}"),
        params![],
    )
    .await
    .unwrap();
    let scoped = with_options(&url, &[("search_path", &schema)]).unwrap();
    let pool = connect(&scoped, super::models(), PoolSettings::new(20))
        .await
        .unwrap();
    artifactory_database::migrate(&pool, &super::MIGRATIONS)
        .await
        .unwrap();
    let store = ControlStore::new(pool.clone());
    let project = Uuid::new_v4();
    let build = Uuid::new_v4();
    let node = Uuid::new_v4();
    let session = Uuid::new_v4();
    let target = json!({"os":"linux","arch":"amd64","steps":[{"name":"compile","run":"true"}],"artifacts":[]});
    sql::execute(&mut pool.clone(), "INSERT INTO projects(id,name,repository_url) VALUES($1,'perf','https://example.invalid/repo.git')", params![project]).await.unwrap();
    sql::execute(&mut pool.clone(), "INSERT INTO builds(id,project_id,requested_ref,status,source_repository_url) VALUES($1,$2,'main','queued','https://example.invalid/repo.git')", params![build, project])
    .await
    .unwrap();
    sql::execute(&mut pool.clone(), "INSERT INTO nodes(id,name,os,arch,pool_id,capabilities,certificate_hash,certificate_expires_at,active_session,last_seen,connection_owner) VALUES($1,'perf','linux','amd64','00000000-0000-0000-0000-000000000001','[\"git\",\"bash\"]',$2,now()+interval '1 day',$3,now(),$3)", params![node, node.to_string(), session]).await.unwrap();
    let mut claim = Vec::new();
    let mut heartbeat = Vec::new();
    let mut event = Vec::new();
    for index in 0..288 {
        let job = Uuid::new_v4();
        sql::execute(
            &mut pool.clone(),
            "INSERT INTO jobs(id,build_id,target,target_config) VALUES($1,$2,$3,$4)",
            params![job, build, index.to_string(), &target],
        )
        .await
        .unwrap();
        let started = Instant::now();
        assert!(store.schedule_one().await.unwrap());
        let (attempt, lease_token): (Uuid, String) = sql::tuple(
            &mut pool.clone(),
            "SELECT id,lease_token FROM attempts WHERE job_id=$1",
            params![job],
        )
        .await
        .unwrap();
        let claim_us = started.elapsed().as_micros() as u64;
        let started = Instant::now();
        store
            .record_heartbeat(
                node,
                session,
                &json!({"capabilities":["git", "bash"], "diagnostics":{}}),
                super::SessionUpdate::Current,
            )
            .await
            .unwrap();
        assert!(
            store
                .renew_attempt(node, session, attempt, &lease_token)
                .await
                .unwrap()
        );
        let heartbeat_us = started.elapsed().as_micros() as u64;
        store
            .submit_execution_event(
                node,
                session,
                attempt,
                ExecutionEvent {
                    kind: "accept",
                    sequence: 0,
                    lease_token: &lease_token,
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
                attempt,
                ExecutionEvent {
                    kind: "step_started",
                    sequence: 1,
                    lease_token: &lease_token,
                    payload_json: "{\"index\":0}",
                },
            )
            .await
            .unwrap();
        let event_us = started.elapsed().as_micros() as u64;
        sql::execute(
            &mut pool.clone(),
            "UPDATE attempts SET status='failed' WHERE id=$1",
            params![attempt],
        )
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
    drop(store);
    drop(pool);
    sql::execute(
        &mut admin.clone(),
        &format!("DROP SCHEMA {schema} CASCADE"),
        params![],
    )
    .await
    .unwrap();
}
