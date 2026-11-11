use artifact_control::{
    api, crypto,
    protocol::Command,
    recipe::Recipe,
    scheduler,
    settings::Settings,
    state::{AppState, Connection},
    storage::Storage,
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{RwLock, mpsc};
use tower::ServiceExt;
use uuid::Uuid;

struct Database {
    state: AppState,
    admin: PgPool,
    schema: String,
}

impl Database {
    async fn new() -> Self {
        let url = std::env::var("AK_TEST_DATABASE_URL")
            .expect("AK_TEST_DATABASE_URL is required for explicit database tests");
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("test_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let search = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .after_connect(move |connection, _| {
                let query = format!("SET search_path TO {search}");
                Box::pin(async move {
                    sqlx::query(&query).execute(connection).await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let settings = Settings {
            database_url: url,
            http_bind: "127.0.0.1:0".into(),
            grpc_bind: "127.0.0.1:0".into(),
            public_url: "http://localhost:3000".into(),
            insecure_dev: true,
            pki_dir: ".local/pki".into(),
            data_dir: std::env::temp_dir(),
            master_key: [1; 32],
            s3_endpoint: "http://localhost:19000".into(),
            s3_public_endpoint: "http://localhost:19000".into(),
            s3_bucket: "test".into(),
            s3_region: "us-east-1".into(),
            s3_access_key: "test".into(),
            s3_secret_key: "test".into(),
        };
        let storage = Arc::new(Storage::new(&settings).unwrap());
        Self {
            state: AppState {
                pool,
                settings: Arc::new(settings),
                storage,
                connections: Arc::new(RwLock::new(HashMap::new())),
            },
            admin,
            schema,
        }
    }
    async fn close(self) {
        self.state.pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&self.admin)
            .await
            .unwrap();
    }
    async fn fixture(&self) -> (Uuid, Uuid, Uuid) {
        let project = Uuid::new_v4();
        let build = Uuid::new_v4();
        let job = Uuid::new_v4();
        let recipe=Recipe::parse("version: 1\nrelease: {enabled: true}\ntargets:\n  app:\n    os: linux\n    arch: amd64\n    steps: [{name: compile, run: make}]\n    artifacts: [dist/app]\n").unwrap();
        sqlx::query("INSERT INTO projects(id,name,repository_url) VALUES($1,'test','https://example.com/repo.git')").bind(project).execute(&self.state.pool).await.unwrap();
        sqlx::query("INSERT INTO builds(id,project_id,requested_ref,commit_sha,config_snapshot,status,release_version,source_repository_url) VALUES($1,$2,'main',$3,$4,'queued','v1','https://example.com/repo.git')").bind(build).bind(project).bind("0".repeat(40)).bind(serde_json::to_value(&recipe).unwrap()).execute(&self.state.pool).await.unwrap();
        sqlx::query("INSERT INTO jobs(id,build_id,target,target_config) VALUES($1,$2,'app',$3)")
            .bind(job)
            .bind(build)
            .bind(serde_json::to_value(&recipe.targets["app"]).unwrap())
            .execute(&self.state.pool)
            .await
            .unwrap();
        (project, build, job)
    }
    async fn node(&self) -> Uuid {
        let node = Uuid::new_v4();
        let session = Uuid::new_v4();
        let (sender, _receiver) = mpsc::channel::<Result<Command, tonic::Status>>(128);
        sqlx::query("INSERT INTO nodes(id,name,os,arch,pool_id,capabilities,certificate_hash,certificate_expires_at,active_session,last_seen) VALUES($1,'test','linux','amd64','00000000-0000-0000-0000-000000000001','[\"git\",\"bash\"]',$2,now()+interval '1 day',$3,now())")
            .bind(node).bind(node.to_string()).bind(session).execute(&self.state.pool).await.unwrap();
        self.state
            .connections
            .write()
            .await
            .insert(node, Connection { session, sender });
        node
    }
}

#[tokio::test]
#[ignore = "requires AK_TEST_DATABASE_URL"]
async fn concurrent_schedulers_do_not_double_assign_or_exceed_slots() {
    let database = Database::new().await;
    let (_, _, job) = database.fixture().await;
    sqlx::query("INSERT INTO jobs(id,build_id,target,target_config) SELECT $1,build_id,'second',target_config FROM jobs WHERE id=$2")
        .bind(Uuid::new_v4()).bind(job).execute(&database.state.pool).await.unwrap();
    database.node().await;
    let (first, second) = tokio::join!(
        scheduler::tick(&database.state),
        scheduler::tick(&database.state)
    );
    first.unwrap();
    second.unwrap();
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM attempts WHERE job_id=$1")
        .bind(job)
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(attempts, 1);
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM attempts")
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(total, 1);
    database.close().await;
}

#[tokio::test]
#[ignore = "requires AK_TEST_DATABASE_URL"]
async fn queued_signing_authorization_is_rechecked() {
    let database = Database::new().await;
    let (project, _, _) = database.fixture().await;
    let node = database.node().await;
    sqlx::query(
        "UPDATE jobs SET target_config=jsonb_set(target_config,'{signing_profiles}','[\"apple\"]')",
    )
    .execute(&database.state.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE nodes SET signing_profiles='{apple}' WHERE id=$1")
        .bind(node)
        .execute(&database.state.pool)
        .await
        .unwrap();
    scheduler::tick(&database.state).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM attempts")
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("UPDATE projects SET signing_profiles='{apple}' WHERE id=$1")
        .bind(project)
        .execute(&database.state.pool)
        .await
        .unwrap();
    scheduler::tick(&database.state).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM attempts")
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    database.close().await;
}

#[tokio::test]
#[ignore = "requires AK_TEST_DATABASE_URL"]
async fn scoped_retry_cannot_enable_publication_and_source_is_snapshotted() {
    let database = Database::new().await;
    let (project, _, job) = database.fixture().await;
    let user = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,password_hash,role) VALUES($1,'builder','unused','operator')").bind(user).execute(&database.state.pool).await.unwrap();
    sqlx::query("INSERT INTO api_tokens(id,user_id,name,token_hash,project_ids,scopes,expires_at) VALUES($1,$2,'build-only',$3,$4,'{read,build}',now()+interval '1 hour')").bind(Uuid::new_v4()).bind(user).bind(crypto::hash("builder")).bind(vec![project]).execute(&database.state.pool).await.unwrap();
    sqlx::query("UPDATE jobs SET status='failed' WHERE id=$1")
        .bind(job)
        .execute(&database.state.pool)
        .await
        .unwrap();
    let router = api::router(database.state.clone());
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/jobs/{job}/retry"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer builder")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(
        router.clone().oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/projects/{project}/builds"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer builder")
        .body(Body::from(r#"{"ref":"main"}"#))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let build = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    sqlx::query("UPDATE projects SET repository_url='https://example.com/changed.git' WHERE id=$1")
        .bind(project)
        .execute(&database.state.pool)
        .await
        .unwrap();
    let repository: String =
        sqlx::query_scalar("SELECT source_repository_url FROM builds WHERE id=$1")
            .bind(build)
            .fetch_one(&database.state.pool)
            .await
            .unwrap();
    assert_eq!(repository, "https://example.com/repo.git");
    database.close().await;
}

#[tokio::test]
#[ignore = "requires AK_TEST_DATABASE_URL"]
async fn incomplete_artifacts_never_make_a_release_visible() {
    let database = Database::new().await;
    let (_, build, _) = database.fixture().await;
    sqlx::query("UPDATE builds SET status='succeeded' WHERE id=$1")
        .bind(build)
        .execute(&database.state.pool)
        .await
        .unwrap();
    assert!(
        scheduler::publish_build(&database.state, build)
            .await
            .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM releases")
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM publish_attempts WHERE status='failed'")
            .fetch_one(&database.state.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    database.close().await;
}

#[tokio::test]
#[ignore = "requires AK_TEST_DATABASE_URL"]
async fn expired_started_tasks_are_lost_and_unstarted_tasks_requeue() {
    let database = Database::new().await;
    let (_, _, job) = database.fixture().await;
    database.node().await;
    scheduler::tick(&database.state).await.unwrap();
    sqlx::query("UPDATE attempts SET lease_expires_at=now()-interval '1 second'")
        .execute(&database.state.pool)
        .await
        .unwrap();
    scheduler::expire(&database.state).await.unwrap();
    let state: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id=$1")
        .bind(job)
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(state, "queued");
    scheduler::tick(&database.state).await.unwrap();
    sqlx::query("UPDATE attempts SET status='running',started_at=now(),lease_expires_at=now()-interval '1 second' WHERE status='leased'").execute(&database.state.pool).await.unwrap();
    scheduler::expire(&database.state).await.unwrap();
    let state: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id=$1")
        .bind(job)
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(state, "lost");
    database.close().await;
}

#[tokio::test]
#[ignore = "requires AK_TEST_DATABASE_URL"]
async fn publication_is_atomic_immutable_and_idempotent() {
    let database = Database::new().await;
    let (_, build, job) = database.fixture().await;
    let node = database.node().await;
    let attempt = Uuid::new_v4();
    let artifact = Uuid::new_v4();
    sqlx::query("UPDATE jobs SET status='succeeded'")
        .execute(&database.state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE builds SET status='succeeded'")
        .execute(&database.state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO attempts(id,job_id,node_id,lease_token,lease_expires_at,status,command_id) VALUES($1,$2,$3,'test',now(),'succeeded',$4)").bind(attempt).bind(job).bind(node).bind(Uuid::new_v4()).execute(&database.state.pool).await.unwrap();
    sqlx::query("INSERT INTO uploads(id,attempt_id,path,object_key,expected_size,expected_sha256,expires_at) VALUES($1,$2,'dist/app',$3,5,$4,now())").bind(artifact).bind(attempt).bind(format!("upload/{artifact}")).bind("a".repeat(64)).execute(&database.state.pool).await.unwrap();
    sqlx::query("INSERT INTO artifacts(id,attempt_id,path,object_key,size,sha256) VALUES($1,$2,'dist/app',$3,5,$4)").bind(artifact).bind(attempt).bind(format!("artifact/{artifact}")).bind("a".repeat(64)).execute(&database.state.pool).await.unwrap();
    let (first, second) = tokio::join!(
        scheduler::publish_build(&database.state, build),
        scheduler::publish_build(&database.state, build)
    );
    assert_eq!(first.unwrap(), second.unwrap());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM releases")
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM release_artifacts")
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    database.close().await;
}

#[tokio::test]
#[ignore = "requires AK_TEST_DATABASE_URL"]
async fn real_http_authorization_rejects_csrf_and_scope_escalation() {
    let database = Database::new().await;
    let (project, _, _) = database.fixture().await;
    let user = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,password_hash,role) VALUES($1,'operator','unused','operator')").bind(user).execute(&database.state.pool).await.unwrap();
    sqlx::query("INSERT INTO sessions VALUES($1,$2,now()+interval '1 hour')")
        .bind(crypto::hash("session"))
        .bind(user)
        .execute(&database.state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO api_tokens(id,user_id,name,token_hash,project_ids,scopes,expires_at) VALUES($1,$2,'scoped',$3,$4,'{read}',now()+interval '1 hour')").bind(Uuid::new_v4()).bind(user).bind(crypto::hash("api-token")).bind(vec![project]).execute(&database.state.pool).await.unwrap();
    let router = api::router(database.state.clone());
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/projects/{project}/builds"))
        .header("content-type", "application/json")
        .header("cookie", "ak_session=session")
        .body(Body::from(r#"{"ref":"main"}"#))
        .unwrap();
    assert_eq!(
        router.clone().oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/projects/{project}/builds"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer api-token")
        .body(Body::from(r#"{"ref":"main"}"#))
        .unwrap();
    assert_eq!(
        router.clone().oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let request = Request::builder()
        .uri("/api/v1/nodes")
        .header("authorization", "Bearer api-token")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router.oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    database.close().await;
}

#[tokio::test]
#[ignore = "requires AK_TEST_DATABASE_URL"]
async fn node_revocation_audit_records_effective_irreversible_state() {
    let database = Database::new().await;
    let node = database.node().await;
    let user = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users(id,username,password_hash,role) VALUES($1,'admin','unused','admin')",
    )
    .bind(user)
    .execute(&database.state.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO sessions VALUES($1,$2,now()+interval '1 hour')")
        .bind(crypto::hash("admin-session"))
        .bind(user)
        .execute(&database.state.pool)
        .await
        .unwrap();
    let router = api::router(database.state.clone());
    for revoked in [true, false] {
        let request = Request::builder()
            .method("PATCH")
            .uri(format!("/api/v1/nodes/{node}"))
            .header("content-type", "application/json")
            .header("cookie", "ak_session=admin-session")
            .header("x-artifact-csrf", "1")
            .header("origin", "http://localhost:3000")
            .body(Body::from(
                serde_json::json!({
                    "draining": true,
                    "revoked": revoked,
                    "slots": 1,
                    "labels": {},
                    "signing_profiles": [],
                    "pool_id": "00000000-0000-0000-0000-000000000001"
                })
                .to_string(),
            ))
            .unwrap();
        assert_eq!(
            router.clone().oneshot(request).await.unwrap().status(),
            StatusCode::OK
        );
    }
    let revoked: bool = sqlx::query_scalar("SELECT revoked FROM nodes WHERE id=$1")
        .bind(node)
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    let states: Vec<bool> = sqlx::query_scalar(
        "SELECT (details->>'revoked')::boolean FROM audit_events WHERE action='node_updated' AND resource_id=$1 ORDER BY id",
    )
    .bind(node.to_string())
    .fetch_all(&database.state.pool)
    .await
    .unwrap();
    assert!(revoked);
    assert_eq!(states, vec![true, true]);
    database.close().await;
}

#[tokio::test]
#[ignore = "requires AK_TEST_DATABASE_URL"]
async fn ota_uses_node_lock_and_pool_limit_and_never_changes_admin_drain() {
    let database = Database::new().await;
    let (_, _, _job) = database.fixture().await;
    let first = database.node().await;
    let second = database.node().await;
    let release = Uuid::new_v4();
    sqlx::query("INSERT INTO agent_releases(id,version,manifest,signature,manifest_hash) VALUES($1,'v-next',$2,'test','hash')")
        .bind(release).bind(serde_json::json!({"assets":[{"os":"linux","arch":"amd64","name":"artifact-agent-linux-amd64"}]}).to_string()).execute(&database.state.pool).await.unwrap();
    sqlx::query("UPDATE nodes SET capabilities=capabilities || '[\"agent-ota-v1\"]'::jsonb,agent_current_version='v-old',agent_launcher_version='v-old',draining=true").execute(&database.state.pool).await.unwrap();
    sqlx::query("UPDATE node_pools SET agent_target_release=$1")
        .bind(release)
        .execute(&database.state.pool)
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        scheduler::tick(&database.state),
        scheduler::tick(&database.state)
    );
    a.unwrap();
    b.unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_updates WHERE status='offered'")
            .fetch_one(&database.state.pool)
            .await
            .unwrap();
    assert_eq!(
        count, 1,
        "pool concurrency must be one across racing schedulers"
    );
    let updates: Vec<Uuid> = sqlx::query_scalar("SELECT node_id FROM agent_updates")
        .fetch_all(&database.state.pool)
        .await
        .unwrap();
    assert!(updates[0] == first || updates[0] == second);
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM attempts")
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0);
    let drained: i64 = sqlx::query_scalar("SELECT count(*) FROM nodes WHERE draining")
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(drained, 2, "OTA must preserve administrator drain");
    database.close().await;
}

#[tokio::test]
#[ignore = "requires AK_TEST_DATABASE_URL"]
async fn ota_management_rejects_operator_and_project_tokens() {
    let database = Database::new().await;
    let (project, _, _) = database.fixture().await;
    let user = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,password_hash,role) VALUES($1,'ota-operator','unused','operator')").bind(user).execute(&database.state.pool).await.unwrap();
    sqlx::query("INSERT INTO sessions VALUES($1,$2,now()+interval '1 hour')")
        .bind(crypto::hash("ota-session"))
        .bind(user)
        .execute(&database.state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO api_tokens(id,user_id,name,token_hash,project_ids,scopes,expires_at) VALUES($1,$2,'scoped',$3,$4,'{read,build,release}',now()+interval '1 hour')").bind(Uuid::new_v4()).bind(user).bind(crypto::hash("ota-token")).bind(vec![project]).execute(&database.state.pool).await.unwrap();
    let router = api::router(database.state.clone());
    for (method, path, body) in [
        (
            "POST",
            "/api/v1/agent-releases".to_string(),
            r#"{"tag":"v-next"}"#.to_string(),
        ),
        (
            "PATCH",
            format!("/api/v1/nodes/{}/agent-update", Uuid::new_v4()),
            r#"{"release_id":null}"#.to_string(),
        ),
        (
            "PATCH",
            "/api/v1/pools/00000000-0000-0000-0000-000000000001/agent-update".into(),
            r#"{"release_id":null}"#.into(),
        ),
        (
            "POST",
            format!("/api/v1/agent-updates/{}/retry", Uuid::new_v4()),
            "{}".into(),
        ),
        (
            "POST",
            format!("/api/v1/agent-updates/{}/cancel", Uuid::new_v4()),
            "{}".into(),
        ),
    ] {
        for token in [false, true] {
            let mut request = Request::builder()
                .method(method)
                .uri(&path)
                .header("content-type", "application/json")
                .header("origin", "http://localhost:3000")
                .header("x-artifact-csrf", "1");
            request = if token {
                request.header("authorization", "Bearer ota-token")
            } else {
                request.header("cookie", "ak_session=ota-session")
            };
            assert_eq!(
                router
                    .clone()
                    .oneshot(request.body(Body::from(body.clone())).unwrap())
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN,
                "{method} {path} token={token}"
            );
        }
    }
    database.close().await;
}

#[tokio::test]
#[ignore = "requires AK_TEST_DATABASE_URL"]
async fn ota_failed_activation_pauses_pool_and_fences_stale_sessions() {
    let database = Database::new().await;
    let node = database.node().await;
    let release = Uuid::new_v4();
    sqlx::query("INSERT INTO agent_releases(id,version,manifest,signature,manifest_hash) VALUES($1,'v-next',$2,'test','hash')").bind(release).bind(serde_json::json!({"assets":[{"os":"linux","arch":"amd64","name":"artifact-agent-linux-amd64"}]}).to_string()).execute(&database.state.pool).await.unwrap();
    sqlx::query("UPDATE nodes SET capabilities=capabilities || '[\"agent-ota-v1\"]'::jsonb,agent_current_version='v-old',agent_target_release=$1,draining=true WHERE id=$2").bind(release).bind(node).execute(&database.state.pool).await.unwrap();
    scheduler::tick(&database.state).await.unwrap();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM agent_updates WHERE node_id=$1")
        .bind(node)
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    let session = database.state.connections.read().await[&node].session;
    let payload = serde_json::json!({"diagnostics":{"agent_version":"v-old"},"ota":{"update_id":id,"phase":"rolled_back","launcher_version":"v-old","error":"candidate crashed"}});
    assert!(
        artifact_control::ota::report(&database.state, node, Uuid::new_v4(), &payload)
            .await
            .is_err()
    );
    let status: String = sqlx::query_scalar("SELECT status FROM agent_updates WHERE id=$1")
        .bind(id)
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(status, "offered");
    let preparing = serde_json::json!({"diagnostics":{"agent_version":"v-old"},"ota":{"update_id":id,"phase":"activating","activation_pending":true,"launcher_version":"v-old"}});
    assert!(
        artifact_control::ota::report(&database.state, node, session, &preparing)
            .await
            .unwrap(),
        "old runtime must receive acknowledgment before yielding to a candidate"
    );
    assert!(
        artifact_control::ota::report(&database.state, node, session, &payload)
            .await
            .unwrap()
    );
    let flags:(bool,bool,bool)=sqlx::query_as("SELECT n.draining,n.ota_draining,p.agent_updates_paused FROM nodes n JOIN node_pools p ON p.id=n.pool_id WHERE n.id=$1").bind(node).fetch_one(&database.state.pool).await.unwrap();
    assert_eq!(flags, (true, false, true));
    scheduler::tick(&database.state).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_updates")
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "failed release must never loop automatically");
    // Revocation and certificate expiry are enforced within the status transaction.
    sqlx::query("UPDATE nodes SET revoked=true WHERE id=$1")
        .bind(node)
        .execute(&database.state.pool)
        .await
        .unwrap();
    assert!(
        artifact_control::ota::report(&database.state, node, session, &payload)
            .await
            .is_err()
    );
    sqlx::query("UPDATE nodes SET revoked=false,certificate_expires_at=now()-interval '1 minute' WHERE id=$1").bind(node).execute(&database.state.pool).await.unwrap();
    assert!(
        artifact_control::ota::report(&database.state, node, session, &payload)
            .await
            .is_err()
    );
    database.close().await;
}

#[tokio::test]
#[ignore = "requires AK_TEST_DATABASE_URL"]
async fn ota_retry_after_pool_move_respects_new_pool_capacity() {
    let database = Database::new().await;
    let node = database.node().await;
    let other = database.node().await;
    let release = Uuid::new_v4();
    let prior = Uuid::new_v4();
    let new_pool = Uuid::new_v4();
    sqlx::query("INSERT INTO agent_releases(id,version,manifest,signature,manifest_hash) VALUES($1,'v-next','{}','test','hash')").bind(release).execute(&database.state.pool).await.unwrap();
    sqlx::query("INSERT INTO agent_updates(id,node_id,pool_id,release_id,status,finished_at) VALUES($1,$2,'00000000-0000-0000-0000-000000000001',$3,'failed',now())").bind(prior).bind(node).bind(release).execute(&database.state.pool).await.unwrap();
    sqlx::query("INSERT INTO node_pools(id,name) VALUES($1,'new pool')")
        .bind(new_pool)
        .execute(&database.state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE nodes SET pool_id=$1")
        .bind(new_pool)
        .execute(&database.state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO agent_updates(id,node_id,pool_id,release_id) VALUES($1,$2,$3,$4)")
        .bind(Uuid::new_v4())
        .bind(other)
        .bind(new_pool)
        .bind(release)
        .execute(&database.state.pool)
        .await
        .unwrap();
    let user = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users(id,username,password_hash,role) VALUES($1,'admin','unused','admin')",
    )
    .bind(user)
    .execute(&database.state.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO sessions VALUES($1,$2,now()+interval '1 hour')")
        .bind(crypto::hash("admin-session"))
        .bind(user)
        .execute(&database.state.pool)
        .await
        .unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/agent-updates/{prior}/retry"))
        .header("content-type", "application/json")
        .header("cookie", "ak_session=admin-session")
        .header("x-artifact-csrf", "1")
        .header("origin", "http://localhost:3000")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(
        api::router(database.state.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    database.close().await;
}
