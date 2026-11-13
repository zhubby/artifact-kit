use super::*;
use artifactory_control::{coordination, dispatch};
use serde_json::{Value, json};

fn replica(state: &AppState) -> AppState {
    AppState {
        coordination: Default::default(),
        connections: Default::default(),
        ..state.clone()
    }
}

async fn connect(state: &AppState, node: Uuid) -> mpsc::Receiver<Result<Command, tonic::Status>> {
    let session = Uuid::new_v4();
    let (sender, receiver) = mpsc::channel(1);
    sqlx::query(
        "UPDATE nodes SET active_session=$2,connection_owner=$3,last_seen=now() WHERE id=$1",
    )
    .bind(node)
    .bind(session)
    .bind(state.coordination.id())
    .execute(&state.pool)
    .await
    .unwrap();
    state
        .connections
        .write()
        .await
        .insert(node, Connection { session, sender });
    receiver
}

async fn redeliver(state: &AppState) {
    sqlx::query("UPDATE device_commands SET next_delivery_at=clock_timestamp()")
        .execute(&state.pool)
        .await
        .unwrap();
    dispatch::deliver(state).await.unwrap();
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn coalesced_notifications_preserve_commands_and_reconciliation() {
    use sqlx::{Acquire, postgres::PgListener};
    use std::time::Duration;

    let database = Database::new().await;
    let (project, build, job) = database.fixture().await;
    let node = database.node().await;
    let mut socket = connect(&database.state, node).await;
    let mut listener = PgListener::connect_with(&database.admin).await.unwrap();
    listener.listen("artifactory_work").await.unwrap();
    let mut notification = database.state.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('artifactory:work-notify',12))")
        .execute(&mut *notification)
        .await
        .unwrap();
    let mut writer = database.state.pool.acquire().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *writer)
        .await
        .unwrap();
    let attempt = Uuid::new_v4();
    let command = Uuid::new_v4();
    let mut tx = writer.begin().await.unwrap();
    sqlx::query("UPDATE jobs SET status=status WHERE id=$1")
        .bind(job)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE projects SET pool_ids=pool_ids WHERE id=$1")
        .bind(project)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE nodes SET slots=slots WHERE id=$1")
        .bind(node)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO attempts(id,job_id,node_id,lease_token,lease_expires_at,status,command_id) VALUES($1,$2,$3,'coalesced',now()+interval '90 seconds','leased',$4)")
        .bind(attempt).bind(job).bind(node).bind(command)
        .execute(&mut *tx).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), tx.commit())
        .await
        .expect("notification contention must not block command commits")
        .unwrap();
    dispatch::deliver(&database.state).await.unwrap();
    let assign = socket.try_recv().unwrap().unwrap();
    assert_eq!(assign.command_id, command.to_string());
    let mut tx = writer.begin().await.unwrap();
    sqlx::query("UPDATE builds SET cancel_requested=true WHERE id=$1")
        .bind(build)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    dispatch::deliver(&database.state).await.unwrap();
    assert_eq!(socket.try_recv().unwrap().unwrap().kind, "cancel");
    let own_notification = tokio::time::timeout(Duration::from_millis(200), async {
        loop {
            if listener.recv().await.unwrap().process_id() == pid as u32 {
                break;
            }
        }
    })
    .await;
    assert!(
        own_notification.is_err(),
        "overlapping work transactions must coalesce their notification, retaining durable commands"
    );
    notification.rollback().await.unwrap();
    sqlx::query("UPDATE nodes SET slots=slots WHERE id=$1")
        .bind(node)
        .execute(&mut *writer)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if listener.recv().await.unwrap().process_id() == pid as u32 {
                break;
            }
        }
    })
    .await
    .expect("work must still notify when no other transaction is notifying");
    drop(writer);
    drop(listener);
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn outbox_is_atomic_and_replays_to_new_owner_with_the_same_command() {
    let database = Database::new().await;
    let (_, _, job) = database.fixture().await;
    let node = database.node().await;
    let mut old = connect(&database.state, node).await;
    let mut tx = database.state.pool.begin().await.unwrap();
    sqlx::query("INSERT INTO attempts(id,job_id,node_id,lease_token,lease_expires_at,status,command_id) VALUES($1,$2,$3,'rollback',now()+interval '90 seconds','leased',$4)")
        .bind(Uuid::new_v4()).bind(job).bind(node).bind(Uuid::new_v4())
        .execute(&mut *tx).await.unwrap();
    tx.rollback().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM device_commands")
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "rolled back leases must never deliver commands");

    scheduler::schedule(&database.state).await.unwrap();
    dispatch::deliver(&database.state).await.unwrap();
    let first = old.try_recv().unwrap().unwrap();
    assert_eq!(first.kind, "assign");
    let next = replica(&database.state);
    let mut current = connect(&next, node).await;
    redeliver(&database.state).await;
    assert!(
        old.try_recv().is_err(),
        "previous replica must not deliver to its stale socket"
    );
    redeliver(&next).await;
    let replay = current.try_recv().unwrap().unwrap();
    assert_eq!(first.command_id, replay.command_id);
    assert_eq!(first.attempt_id, replay.attempt_id);
    assert_eq!(first.lease_token, replay.lease_token);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM attempts")
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(
        count, 1,
        "reconnect must retain the original fenced attempt"
    );
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn full_command_queue_and_cancel_are_replayed_until_durable_ack() {
    let database = Database::new().await;
    let (_, build, _) = database.fixture().await;
    let node = database.node().await;
    let mut socket = connect(&database.state, node).await;
    let sender = database.state.connections.read().await[&node]
        .sender
        .clone();
    sender.try_send(Ok(Command::default())).unwrap();
    scheduler::schedule(&database.state).await.unwrap();
    dispatch::deliver(&database.state).await.unwrap();
    socket.try_recv().unwrap().unwrap();
    redeliver(&database.state).await;
    let assign = socket.try_recv().unwrap().unwrap();
    let attempt = Uuid::parse_str(&assign.attempt_id).unwrap();
    sqlx::query("UPDATE attempts SET status='running' WHERE id=$1")
        .bind(attempt)
        .execute(&database.state.pool)
        .await
        .unwrap();
    redeliver(&database.state).await;
    assert!(
        socket.try_recv().is_err(),
        "running is the durable assignment ACK"
    );
    sqlx::query("UPDATE builds SET cancel_requested=true WHERE id=$1")
        .bind(build)
        .execute(&database.state.pool)
        .await
        .unwrap();
    redeliver(&database.state).await;
    let cancel = socket.try_recv().unwrap().unwrap();
    assert_eq!(cancel.kind, "cancel");
    redeliver(&database.state).await;
    assert_eq!(
        socket.try_recv().unwrap().unwrap().command_id,
        cancel.command_id
    );
    sqlx::query("UPDATE attempts SET status='canceled' WHERE id=$1")
        .bind(attempt)
        .execute(&database.state.pool)
        .await
        .unwrap();
    redeliver(&database.state).await;
    assert!(socket.try_recv().is_err());
    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM device_commands WHERE acknowledged_at IS NULL")
            .fetch_one(&database.state.pool)
            .await
            .unwrap();
    assert_eq!(pending, 0);
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn three_replicas_competing_for_distinct_builds_do_not_overbook_a_node() {
    let database = Database::new().await;
    for _ in 0..20 {
        database.fixture().await;
    }
    let node = database.node().await;
    sqlx::query("UPDATE nodes SET slots=3 WHERE id=$1")
        .bind(node)
        .execute(&database.state.pool)
        .await
        .unwrap();
    let second = replica(&database.state);
    let third = replica(&database.state);
    let (a, b, c) = tokio::join!(
        scheduler::schedule(&database.state),
        scheduler::schedule(&second),
        scheduler::schedule(&third)
    );
    a.unwrap();
    b.unwrap();
    c.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM attempts")
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 3);
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn draining_ownership_stops_other_replicas_from_admitting_new_work() {
    let database = Database::new().await;
    let (_, _, job) = database.fixture().await;
    let node = database.node().await;
    let session: Uuid = sqlx::query_scalar("SELECT active_session FROM nodes WHERE id=$1")
        .bind(node)
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    let other = replica(&database.state);
    coordination::drain(&database.state).await.unwrap();
    scheduler::schedule(&other).await.unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id=$1")
        .bind(job)
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(status, "queued");
    let retained: Uuid = sqlx::query_scalar("SELECT active_session FROM nodes WHERE id=$1")
        .bind(node)
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(
        session, retained,
        "existing leases keep their authorized session during drain"
    );
    let _socket = connect(&other, node).await;
    sqlx::query("UPDATE jobs SET dispatch_after=now() WHERE id=$1")
        .bind(job)
        .execute(&database.state.pool)
        .await
        .unwrap();
    scheduler::schedule(&other).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM attempts")
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let router = api::router(database.state.clone());
    let request = Request::builder()
        .uri("/readyz")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router.oneshot(request).await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn draining_a_locked_node_does_not_retain_locks_on_other_nodes() {
    let database = Database::new().await;
    let locked = database.node().await;
    let available = database.node().await;
    let mut lock = database.state.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(locked)
        .execute(&mut *lock)
        .await
        .unwrap();
    let state = database.state.clone();
    let drain = tokio::spawn(async move { coordination::drain(&state).await });
    let progress = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let cleared: bool =
                sqlx::query_scalar("SELECT connection_owner IS NULL FROM nodes WHERE id=$1")
                    .bind(available)
                    .fetch_one(&database.state.pool)
                    .await
                    .unwrap();
            if cleared {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    lock.rollback().await.unwrap();
    drain.await.unwrap().unwrap();
    database.close().await;
    assert!(
        progress.is_ok(),
        "a busy node must not hold every other node's ownership until its lock is released"
    );
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn locked_build_maintenance_does_not_block_new_job_admission() {
    let database = Database::new().await;
    let (_, build, job) = database.fixture().await;
    sqlx::query("UPDATE jobs SET status='succeeded' WHERE id=$1")
        .bind(job)
        .execute(&database.state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE builds SET status='running' WHERE id=$1")
        .bind(build)
        .execute(&database.state.pool)
        .await
        .unwrap();
    database.node().await;
    let mut lock = database.state.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM builds WHERE id=$1 FOR UPDATE")
        .bind(build)
        .execute(&mut *lock)
        .await
        .unwrap();
    scheduler::start(database.state.clone());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity a JOIN pg_locks l ON l.pid=a.pid
                 JOIN pg_class c ON c.oid=l.relation JOIN pg_namespace n ON n.oid=c.relnamespace
                 WHERE n.nspname=$1 AND c.relname='builds' AND a.wait_event_type='Lock'
                   AND a.query LIKE 'WITH summaries AS%')",
            )
            .bind(&database.schema)
            .fetch_one(&database.admin)
            .await
            .unwrap();
            if blocked {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("maintenance must reach the locked build before testing admission");
    let (_, _, available) = database.fixture().await;
    let admitted = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id=$1")
                .bind(available)
                .fetch_one(&database.state.pool)
                .await
                .unwrap();
            if status == "leased" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    lock.rollback().await.unwrap();
    database.state.coordination.stop();
    database.close().await;
    assert!(
        admitted.is_ok(),
        "a locked build summary must not block admission of unrelated queued jobs"
    );
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn incompatible_queue_prefix_does_not_starve_compatible_work() {
    let database = Database::new().await;
    let (_, _, base) = database.fixture().await;
    sqlx::query(
        "UPDATE jobs SET target_config=jsonb_set(target_config,'{os}','\"windows\"') WHERE id=$1",
    )
    .bind(base)
    .execute(&database.state.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO jobs(id,build_id,target,target_config) SELECT gen_random_uuid(),build_id,'incompatible-'||s,target_config FROM jobs CROSS JOIN generate_series(1,129) s WHERE id=$1")
        .bind(base).execute(&database.state.pool).await.unwrap();
    let (_, _, compatible) = database.fixture().await;
    database.node().await;
    for _ in 0..3 {
        scheduler::schedule(&database.state).await.unwrap();
    }
    let status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id=$1")
        .bind(compatible)
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(status, "leased");
    database.close().await;
}

async fn read_nodes(state: &AppState, query: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(format!("/api/v1/nodes{query}"))
        .header("cookie", "artifactory_session=nodes-session")
        .body(Body::empty())
        .unwrap();
    let response = api::router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn node_pagination_is_bounded_stable_searchable_and_hides_sessions() {
    let database = Database::new().await;
    let user = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users(id,username,password_hash,role) VALUES($1,'viewer','unused','viewer')",
    )
    .bind(user)
    .execute(&database.state.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO sessions VALUES($1,$2,now()+interval '1 hour')")
        .bind(crypto::hash("nodes-session"))
        .bind(user)
        .execute(&database.state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO nodes(id,name,os,arch,pool_id,certificate_hash,certificate_expires_at,created_at) SELECT gen_random_uuid(),'device-'||s,'linux','amd64','00000000-0000-0000-0000-000000000001',s::text,now()+interval '1 day',now() FROM generate_series(1,250) s")
        .execute(&database.state.pool).await.unwrap();
    let (status, first) = read_nodes(&database.state, "").await;
    assert_eq!(status, StatusCode::OK);
    let first = first.as_array().unwrap();
    assert_eq!(first.len(), 100);
    let (_, second) = read_nodes(&database.state, "?offset=100&limit=100").await;
    let second = second.as_array().unwrap();
    assert_eq!(second.len(), 100);
    assert!(
        first
            .iter()
            .all(|node| second.iter().all(|other| node["id"] != other["id"]))
    );
    for node in first {
        for key in ["active_session", "certificate_hash", "connection_owner"] {
            assert!(node.get(key).is_none(), "leaked {key}");
        }
    }
    let (_, matching) = read_nodes(&database.state, "?q=device-250").await;
    assert_eq!(matching.as_array().unwrap().len(), 1);
    for query in ["?limit=0", "?limit=501", "?offset=-1"] {
        assert_eq!(
            read_nodes(&database.state, query).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn planner_groups_large_fleets_and_retains_tool_versions() {
    let database = Database::new().await;
    let (_, build, _) = database.fixture().await;
    sqlx::query("UPDATE builds SET planning_context=$2 WHERE id=$1")
        .bind(build)
        .bind(json!({"pool_ids":["00000000-0000-0000-0000-000000000001"]}))
        .execute(&database.state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO nodes(id,name,os,arch,pool_id,certificate_hash,certificate_expires_at,diagnostics) SELECT gen_random_uuid(),'device-'||s,'linux','amd64','00000000-0000-0000-0000-000000000001',s::text,now()+interval '1 day','{\"tools\":{\"rust\":\"1.91\"}}'::jsonb FROM generate_series(1,10000) s")
        .execute(&database.state.pool).await.unwrap();
    let summary = artifactory_control::planning::nodes(&database.state, build)
        .await
        .unwrap();
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0]["count"], 10000);
    assert_eq!(summary[0]["tools"]["rust"], "1.91");
    assert_eq!(summary[0]["truncated"], false);
    sqlx::query("UPDATE nodes SET diagnostics=jsonb_build_object('tools',jsonb_build_object('version',id::text))")
        .execute(&database.state.pool).await.unwrap();
    let summary = artifactory_control::planning::nodes(&database.state, build)
        .await
        .unwrap();
    assert_eq!(summary.len(), 128);
    assert_eq!(summary[0]["total_groups"], 10000);
    assert_eq!(summary[0]["truncated"], true);
    sqlx::query("DELETE FROM nodes")
        .execute(&database.state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO nodes(id,name,os,arch,pool_id,certificate_hash,certificate_expires_at,diagnostics) SELECT gen_random_uuid(),'verbose-'||s,'linux','amd64','00000000-0000-0000-0000-000000000001',s::text,now()+interval '1 day',jsonb_build_object('tools',jsonb_build_object('version',repeat('v',6000)||s::text)) FROM generate_series(1,50) s")
        .execute(&database.state.pool).await.unwrap();
    let summary = artifactory_control::planning::nodes(&database.state, build)
        .await
        .unwrap();
    assert!(
        serde_json::to_vec(&summary).unwrap().len() <= 128 * 1024,
        "verbose tool reports must fit the planner context budget"
    );
    assert_eq!(summary[0]["total_groups"], 50);
    assert_eq!(summary[0]["truncated"], true);
    sqlx::query("UPDATE nodes SET diagnostics=jsonb_build_object('tools',jsonb_build_object('version',repeat('v',40000)))")
        .execute(&database.state.pool).await.unwrap();
    let summary = artifactory_control::planning::nodes(&database.state, build)
        .await
        .unwrap();
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0]["count"], 50);
    assert_eq!(summary[0]["details_omitted"], true);
    assert_eq!(summary[0]["truncated"], true);
    database.close().await;
}
