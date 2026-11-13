use artifactory_control_plane::{
    api, crypto,
    protocol::Command,
    recipe::Recipe,
    scheduler,
    settings::Settings,
    state::{AppState, Connection},
    storage::Storage,
};
use artifactory_database::{PoolSettings, connect, with_options};
use artifactory_database::{params, sql};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{RwLock, mpsc};
use tower::ServiceExt;
use uuid::Uuid;

#[path = "database/registration.rs"]
mod registration;
#[path = "database/runtime.rs"]
mod runtime;
#[path = "database/source_refs.rs"]
mod source_refs;

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL and git-lfs"]
async fn evicted_source_restores_pinned_objects_after_upstream_refs_move() {
    use artifactory_control_plane::source_cache::{self, LfsObject, Repository, Snapshot};
    use tokio::io::{AsyncBufReadExt, BufReader};
    let database = Database::new().await;
    let (project, build, _) = database.fixture().await;
    let upstream = tempfile::tempdir().unwrap();
    let origin = upstream.path().join("repo.git");
    tokio::fs::create_dir(&origin).await.unwrap();
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.name", "Source test"],
        vec!["config", "user.email", "source@localhost"],
        vec!["config", "commit.gpgsign", "false"],
        vec!["config", "filter.lfs.clean", "git-lfs clean -- %f"],
        vec!["config", "filter.lfs.smudge", "git-lfs smudge -- %f"],
        vec!["config", "filter.lfs.process", "git-lfs filter-process"],
        vec!["config", "filter.lfs.required", "true"],
    ] {
        source_cache::git(&origin, &args).await.unwrap();
    }
    let payload = b"original-lfs-payload\n";
    tokio::fs::write(
        origin.join(".gitattributes"),
        b"payload.dat filter=lfs diff=lfs merge=lfs -text\n",
    )
    .await
    .unwrap();
    tokio::fs::write(origin.join("payload.dat"), payload)
        .await
        .unwrap();
    tokio::fs::write(origin.join("main.c"), b"original source\n")
        .await
        .unwrap();
    source_cache::git(&origin, &["add", "."]).await.unwrap();
    source_cache::git(&origin, &["commit", "-m", "original"])
        .await
        .unwrap();
    source_cache::git(&origin, &["tag", "-a", "frozen-v1", "-m", "original tag"])
        .await
        .unwrap();
    let commit = String::from_utf8(
        source_cache::git(&origin, &["rev-parse", "HEAD"])
            .await
            .unwrap(),
    )
    .unwrap()
    .trim()
    .to_owned();
    let tag = String::from_utf8(
        source_cache::git(&origin, &["rev-parse", "frozen-v1"])
            .await
            .unwrap(),
    )
    .unwrap()
    .trim()
    .to_owned();
    // Retain the old tag through another annotated tag, so both original
    // objects remain reachable without being advertised as reference tips.
    source_cache::git(
        &origin,
        &["tag", "-a", "archive-v1", "frozen-v1", "-m", "retained tag"],
    )
    .await
    .unwrap();
    tokio::fs::write(origin.join("payload.dat"), b"new-lfs-payload\n")
        .await
        .unwrap();
    tokio::fs::write(origin.join("main.c"), b"new source\n")
        .await
        .unwrap();
    source_cache::git(&origin, &["add", "."]).await.unwrap();
    source_cache::git(&origin, &["commit", "-m", "moved branch"])
        .await
        .unwrap();
    source_cache::git(
        &origin,
        &["tag", "-f", "-a", "frozen-v1", "-m", "moved tag"],
    )
    .await
    .unwrap();
    let refs = source_cache::git(&origin, &["show-ref"]).await.unwrap();
    let mut server = tokio::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/source_http.py"
        ))
        .arg(upstream.path())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut port = String::new();
    BufReader::new(server.stdout.take().unwrap())
        .read_line(&mut port)
        .await
        .unwrap();
    let repository = Uuid::new_v4();
    sql::execute(&mut database.pool.clone(), "INSERT INTO source_repositories(id,project_id,repository_url,cache_key) VALUES($1,$2,$3,$4)", params![repository, project, format!("http://127.0.0.1:{}/repo.git", port.trim()), crypto::hash(repository.to_string())]).await.unwrap();
    let snapshot = Snapshot {
        id: Uuid::new_v4(),
        project_id: project,
        commit: commit.clone(),
        repositories: vec![Repository {
            id: Uuid::new_v4(),
            repository_id: repository,
            path: String::new(),
            commit: commit.clone(),
            tags: [("frozen-v1".into(), tag.clone())].into(),
            lfs: vec![LfsObject {
                path: "payload.dat".into(),
                oid: crypto::hash(payload),
                size: payload.len() as u64,
            }],
        }],
    };
    sql::execute(&mut database.pool.clone(), "INSERT INTO source_snapshots(id,project_id,root_repository_id,commit_sha,manifest,digest) VALUES($1,$2,$3,$4,$5,$6)", params![snapshot.id, project, repository, &commit, serde_json::to_value(&snapshot).unwrap(), snapshot.id.to_string()]).await.unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE builds SET source_snapshot_id=$2,status='running' WHERE id=$1",
        params![build, snapshot.id],
    )
    .await
    .unwrap();
    let repo = source_cache::repository_path(&database.state, repository)
        .await
        .unwrap();
    for _ in 0..2 {
        source_cache::ensure_available(&database.state, &snapshot)
            .await
            .unwrap();
        let prefix = format!(
            "refs/namespaces/{}/refs",
            source_cache::namespace(snapshot.id, snapshot.repositories[0].id)
        );
        assert_eq!(
            source_cache::git(&repo, &["show", &format!("{prefix}/heads/source:main.c")])
                .await
                .unwrap(),
            b"original source\n"
        );
        assert_eq!(
            String::from_utf8(
                source_cache::git(&repo, &["rev-parse", &format!("{prefix}/tags/frozen-v1")])
                    .await
                    .unwrap()
            )
            .unwrap()
            .trim(),
            tag
        );
        assert_eq!(
            tokio::fs::read(source_cache::lfs_path(&repo, &crypto::hash(payload)).unwrap())
                .await
                .unwrap(),
            payload
        );
        source_cache::collect(&database.state, 0).await.unwrap();
        assert!(
            repo.join("HEAD").exists(),
            "running snapshot must remain protected"
        );
        sql::execute(
            &mut database.pool.clone(),
            "UPDATE builds SET status='succeeded' WHERE id=$1",
            params![build],
        )
        .await
        .unwrap();
        source_cache::collect(&database.state, 0).await.unwrap();
        assert!(!repo.exists(), "terminal cache should be evicted");
        sql::execute(
            &mut database.pool.clone(),
            "UPDATE builds SET status='running' WHERE id=$1",
            params![build],
        )
        .await
        .unwrap();
    }
    assert_eq!(
        source_cache::git(&origin, &["show-ref"]).await.unwrap(),
        refs
    );
    source_cache::git(&origin, &["tag", "-d", "archive-v1"])
        .await
        .unwrap();
    source_cache::git(&origin, &["reflog", "expire", "--expire=now", "--all"])
        .await
        .unwrap();
    source_cache::git(&origin, &["gc", "--prune=now"])
        .await
        .unwrap();
    let error = source_cache::ensure_available(&database.state, &snapshot)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("pinned tag object is no longer available upstream"));
    let _ = tokio::fs::remove_dir_all(repo).await;
    server.kill().await.unwrap();
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn source_download_rechecks_authorization_after_cache_lock() {
    use artifactory_control_plane::source_cache::{self, LfsObject, Repository, Snapshot};
    let database = Database::new().await;
    let (project, build, job) = database.fixture().await;
    let node = database.node().await;
    let repository = Uuid::new_v4();
    let entry = Uuid::new_v4();
    let snapshot = Uuid::new_v4();
    let attempt = Uuid::new_v4();
    let key = crypto::hash(repository.to_string());
    let oid = crypto::hash("data");
    sql::execute(&mut database.pool.clone(), "INSERT INTO source_repositories(id,project_id,repository_url,cache_key) VALUES($1,$2,'https://example.com/repo.git',$3)", params![repository, project, &key]).await.unwrap();
    let manifest = Snapshot {
        id: snapshot,
        project_id: project,
        commit: "0".repeat(40),
        repositories: vec![Repository {
            id: entry,
            repository_id: repository,
            path: String::new(),
            commit: "0".repeat(40),
            tags: Default::default(),
            lfs: vec![LfsObject {
                path: "payload.dat".into(),
                oid: oid.clone(),
                size: 4,
            }],
        }],
    };
    sql::execute(&mut database.pool.clone(), "INSERT INTO source_snapshots(id,project_id,root_repository_id,commit_sha,manifest,digest) VALUES($1,$2,$3,$4,$5,$6)", params![snapshot, project, repository, "0".repeat(40), serde_json::to_value(manifest).unwrap(), snapshot.to_string()]).await.unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE builds SET source_snapshot_id=$2,status='running' WHERE id=$1",
        params![build, snapshot],
    )
    .await
    .unwrap();
    sql::execute(&mut database.pool.clone(), "INSERT INTO attempts(id,job_id,node_id,lease_token,lease_expires_at,status,command_id) VALUES($1,$2,$3,'lease',now()+interval '1 minute','running',$4)", params![attempt, job, node, Uuid::new_v4()]).await.unwrap();
    sql::execute(&mut database.pool.clone(), "INSERT INTO source_tokens(token_hash,snapshot_id,attempt_id,expires_at) VALUES($1,$2,$3,now()+interval '1 hour')", params![crypto::hash("download-token"), snapshot, attempt]).await.unwrap();
    let repo = source_cache::repository_path(&database.state, repository)
        .await
        .unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    let lfs = source_cache::lfs_path(&repo, &oid).unwrap();
    tokio::fs::create_dir_all(lfs.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&lfs, b"data").await.unwrap();
    let router = api::router(database.state.clone());
    let mut actual = Vec::new();
    for kind in ["git", "lfs"] {
        for invalidation in ["cancel", "revoke", "expire"] {
            sql::execute(
                &mut database.pool.clone(),
                "UPDATE builds SET cancel_requested=false WHERE id=$1",
                params![build],
            )
            .await
            .unwrap();
            sql::execute(
                &mut database.pool.clone(),
                "UPDATE nodes SET revoked=false WHERE id=$1",
                params![node],
            )
            .await
            .unwrap();
            sql::execute(
                &mut database.pool.clone(),
                "UPDATE attempts SET lease_expires_at=now()+interval '1 minute' WHERE id=$1",
                params![attempt],
            )
            .await
            .unwrap();
            let suffix = if kind == "git" {
                "git/info/refs?service=git-upload-pack".to_string()
            } else {
                format!("lfs/objects/{oid}")
            };
            let request = || {
                Request::builder()
                    .uri(format!("/api/v1/source/{snapshot}/{entry}/{suffix}"))
                    .header("authorization", "Bearer download-token")
                    .body(Body::empty())
                    .unwrap()
            };
            let valid = router.clone().oneshot(request()).await.unwrap();
            assert_eq!(valid.status(), StatusCode::OK);
            axum::body::to_bytes(valid.into_body(), 65536)
                .await
                .unwrap();
            let mut lock_db = database.pool.clone();
            let mut lock = lock_db.transaction().await.unwrap();
            let backend: i32 = sql::scalar(&mut lock, "SELECT pg_backend_pid()", params![])
                .await
                .unwrap();
            sql::execute(
                &mut lock,
                "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
                params![&key],
            )
            .await
            .unwrap();
            let waiting_router = router.clone();
            let pending = request();
            let download =
                tokio::spawn(async move { waiting_router.oneshot(pending).await.unwrap() });
            let mut blocked = false;
            for _ in 0..100 {
                blocked = sql::scalar(&mut database.pool.clone(), "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))", params![backend]).await.unwrap();
                if blocked {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(
                blocked,
                "source request must be waiting on the real cache lock"
            );
            let (query, id) = match invalidation {
                "cancel" => ("UPDATE builds SET cancel_requested=true WHERE id=$1", build),
                "revoke" => ("UPDATE nodes SET revoked=true WHERE id=$1", node),
                _ => (
                    "UPDATE attempts SET lease_expires_at=now()-interval '1 second' WHERE id=$1",
                    attempt,
                ),
            };
            sql::execute(&mut database.pool.clone(), query, params![id])
                .await
                .unwrap();
            lock.commit().await.unwrap();
            actual.push(download.await.unwrap().status());
        }
    }
    tokio::fs::remove_dir_all(repo).await.unwrap();
    database.close().await;
    assert_eq!(actual, vec![StatusCode::UNAUTHORIZED; 6]);
}

async fn planning_fixture(database: &Database) -> (Uuid, Uuid, Uuid) {
    let (_, build, _) = database.fixture().await;
    let worker = Uuid::new_v4();
    let run = Uuid::new_v4();
    sql::execute(&mut database.pool.clone(), "INSERT INTO ai_workers(id,name,certificate_hash,certificate_expires_at) VALUES($1,'planner',$2,now()+interval '1 day')", params![worker, worker.to_string()]).await.unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE builds SET status='planning',goal='Build project' WHERE id=$1",
        params![build],
    )
    .await
    .unwrap();
    sql::execute(&mut database.pool.clone(), "INSERT INTO planning_runs(id,build_id,worker_id,purpose,state,model_config,lease_hash,lease_expires_at,deadline_at) VALUES($1,$2,$3,'initial','leased','{}',$4,now()-interval '1 second',now()+interval '5 minutes')", params![run, build, worker, crypto::hash("lease")]).await.unwrap();
    (build, run, worker)
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn planner_reaper_rechecks_renewal_after_waiting_for_build_lock() {
    let database = Database::new().await;
    let (build, run, _) = planning_fixture(&database).await;
    let mut lock_db = database.pool.clone();
    let mut lock = lock_db.transaction().await.unwrap();
    sql::execute(
        &mut lock,
        "SELECT id FROM builds WHERE id=$1 FOR UPDATE",
        params![build],
    )
    .await
    .unwrap();
    let state = database.state.clone();
    let reaper =
        tokio::spawn(async move { artifactory_control_plane::planning::reap(&state).await });
    let mut blocked = false;
    for _ in 0..100 {
        blocked=sql::scalar(&mut database.pool.clone(), "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event_type='Lock' AND query LIKE 'SELECT id FROM builds WHERE id=$1 FOR UPDATE%')", params![]).await.unwrap();
        if blocked {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(blocked, "reaper did not wait for the build lock");
    sql::execute(
        &mut lock,
        "UPDATE planning_runs SET lease_expires_at=now()+interval '1 minute' WHERE id=$1",
        params![run],
    )
    .await
    .unwrap();
    lock.commit().await.unwrap();
    reaper.await.unwrap().unwrap();
    let state: String = sql::scalar(
        &mut database.pool.clone(),
        "SELECT state FROM planning_runs WHERE id=$1",
        params![run],
    )
    .await
    .unwrap();
    assert_eq!(state, "leased");
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE planning_runs SET lease_expires_at=now()-interval '1 second' WHERE id=$1",
        params![run],
    )
    .await
    .unwrap();
    artifactory_control_plane::planning::reap(&database.state)
        .await
        .unwrap();
    let state: (String, Option<String>) = sql::tuple(
        &mut database.pool.clone(),
        "SELECT state,lease_hash FROM planning_runs WHERE id=$1",
        params![run],
    )
    .await
    .unwrap();
    assert_eq!(state, ("queued".into(), None));
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn planner_deadline_finishes_build_and_failed_planning_does_not_loop() {
    let database = Database::new().await;
    let (build, run, _) = planning_fixture(&database).await;
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE planning_runs SET deadline_at=now()-interval '1 second' WHERE id=$1",
        params![run],
    )
    .await
    .unwrap();
    artifactory_control_plane::planning::reap(&database.state)
        .await
        .unwrap();
    let state:(String,String)=sql::tuple(&mut database.pool.clone(), "SELECT b.status,r.state FROM builds b JOIN planning_runs r ON r.build_id=b.id WHERE b.id=$1", params![build]).await.unwrap();
    assert_eq!(state, ("failed".into(), "failed".into()));
    scheduler::queue_revisions(&database.state).await.unwrap();
    let count: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM planning_runs WHERE build_id=$1",
        params![build],
    )
    .await
    .unwrap();
    assert_eq!(count, 1);
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn model_profile_queries_mask_keys_and_validate_runtime_parameters() {
    let database = Database::new().await;
    let user = Uuid::new_v4();
    sql::execute(
        &mut database.pool.clone(),
        "INSERT INTO users(id,username,password_hash,role) VALUES($1,'admin','unused','admin')",
        params![user],
    )
    .await
    .unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "INSERT INTO sessions VALUES($1,$2,now()+interval '1 hour')",
        params![crypto::hash("ai-session"), user],
    )
    .await
    .unwrap();
    let router = api::router(database.state.clone());
    for (parameters, expected) in [
        (
            serde_json::json!({"max_tokens":-1}),
            StatusCode::BAD_REQUEST,
        ),
        (
            serde_json::json!({"thinking_level":"invalid"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            serde_json::json!({"headers":{"Authorization":"override"}}),
            StatusCode::BAD_REQUEST,
        ),
        (
            serde_json::json!({"max_tokens":1024,"requests_per_minute":3}),
            StatusCode::OK,
        ),
    ] {
        let body = serde_json::json!({"name":"real","api":"openai-completions","base_url":"https://model.example/v1","model":"test","api_key":"private-model-key","parameters":parameters,"is_default":true});
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/model-profiles")
                    .header("cookie", "artifactory_session=ai-session")
                    .header("origin", "http://localhost:3000")
                    .header("x-artifactory-csrf", "1")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    let response = router
        .oneshot(
            Request::builder()
                .uri("/api/v1/model-profiles")
                .header("cookie", "artifactory_session=ai-session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(!body.contains("private-model-key"));
    assert!(!body.contains("key_ciphertext"));
    assert!(body.contains("********"));
    let ciphertext: String = sql::scalar(
        &mut database.pool.clone(),
        "SELECT key_ciphertext FROM model_profiles",
        params![],
    )
    .await
    .unwrap();
    assert_ne!(ciphertext, "private-model-key");
    assert_eq!(
        crypto::open(&database.state.settings.master_key, &ciphertext).unwrap(),
        b"private-model-key"
    );
    database.close().await;
}

struct Database {
    state: AppState,
    pool: toasty::Db,
    admin: toasty::Db,
    schema: String,
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn registration_creates_viewer_and_standard_session() {
    use argon2::{Argon2, PasswordHash, PasswordVerifier};
    let database = Database::new().await;
    let router = api::router(database.state.clone());
    let password = "a-new-reader-password";
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/register")
                .header("content-type", "application/json")
                .header("x-artifactory-csrf", "1")
                .header("origin", "http://localhost:3000")
                .body(Body::from(
                    serde_json::json!({"username":"  new-reader  ","password":password})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let set_cookie = response.headers()["set-cookie"].to_str().unwrap();
    assert!(set_cookie.contains("HttpOnly; SameSite=Strict; Path=/; Max-Age=86400"));
    assert!(!set_cookie.contains("Secure"));
    let cookie = set_cookie.split(';').next().unwrap().to_owned();
    let body = axum::body::to_bytes(response.into_body(), 8192)
        .await
        .unwrap();
    let user: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(user["username"], "new-reader");
    assert_eq!(user["role"], "viewer");
    assert!(user.get("password_hash").is_none());
    let (encoded, role): (String, String) = sql::tuple(
        &mut database.pool.clone(),
        "SELECT password_hash,role FROM users WHERE username='new-reader'",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(role, "viewer");
    assert_ne!(encoded, password);
    Argon2::default()
        .verify_password(password.as_bytes(), &PasswordHash::new(&encoded).unwrap())
        .unwrap();
    let sessions: i64 = sql::scalar(&mut database.pool.clone(), "SELECT count(*) FROM sessions WHERE expires_at>now()+interval '23 hours' AND expires_at<=now()+interval '24 hours'", params![])
    .await
    .unwrap();
    assert_eq!(sessions, 1);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let audit: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM audit_events WHERE action='user_registered'",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(audit, 1);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/logout")
                .header("cookie", &cookie)
                .header("x-artifactory-csrf", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header("content-type", "application/json")
                .header("x-artifactory-csrf", "1")
                .body(Body::from(
                    serde_json::json!({"username":"new-reader","password":password}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key("set-cookie"));
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn revision_budget_excludes_source_authorization_and_transport_failures() {
    let database = Database::new().await;
    let node = database.node().await;
    for failure in [
        "execution",
        "source",
        "authorization",
        "artifact_transport",
        "emissary",
    ] {
        let (project, build, job) = database.fixture().await;
        let repo = Uuid::new_v4();
        let snapshot = Uuid::new_v4();
        let run = Uuid::new_v4();
        let plan = Uuid::new_v4();
        sql::execute(&mut database.pool.clone(), "INSERT INTO source_repositories(id,project_id,repository_url,cache_key) VALUES($1,$2,'https://example.com/source.git',$3)", params![repo, project, repo.to_string()]).await.unwrap();
        sql::execute(&mut database.pool.clone(), "INSERT INTO source_snapshots(id,project_id,root_repository_id,commit_sha,manifest,digest) VALUES($1,$2,$3,$4,'{}',$5)", params![snapshot, project, repo, "0".repeat(40), snapshot.to_string()]).await.unwrap();
        sql::execute(&mut database.pool.clone(), "INSERT INTO planning_runs(id,build_id,purpose,state,model_config) VALUES($1,$2,'initial','succeeded','{}')", params![run, build]).await.unwrap();
        sql::execute(&mut database.pool.clone(), "INSERT INTO plan_revisions(id,build_id,run_id,revision,source_snapshot_id,contents,sha256) VALUES($1,$2,$3,0,$4,'{}','digest')", params![plan, build, run, snapshot]).await.unwrap();
        sql::execute(&mut database.pool.clone(), "UPDATE builds SET goal='Build CLI',model_config_snapshot='{}',current_plan_id=$2,source_snapshot_id=$3,status='failed' WHERE id=$1", params![build, plan, snapshot]).await.unwrap();
        sql::execute(
            &mut database.pool.clone(),
            "UPDATE jobs SET status='failed',plan_revision_id=$2 WHERE id=$1",
            params![job, plan],
        )
        .await
        .unwrap();
        sql::execute(&mut database.pool.clone(), "INSERT INTO attempts(id,job_id,node_id,lease_token,lease_expires_at,status,command_id,error,failure_kind) VALUES($1,$2,$3,'test',now(),'failed',$4,'controlled failure',$5)", params![Uuid::new_v4(), job, node, Uuid::new_v4(), failure]).await.unwrap();
        scheduler::queue_revisions(&database.state).await.unwrap();
        let (status, count): (String, i32) = sql::tuple(
            &mut database.pool.clone(),
            "SELECT status,revision_count FROM builds WHERE id=$1",
            params![build],
        )
        .await
        .unwrap();
        if failure == "execution" {
            assert_eq!((status, count), ("replanning".into(), 1));
            // A later execution failure can consume the second revision, then
            // the same scheduler must leave the terminal failure untouched.
            sql::execute(
                &mut database.pool.clone(),
                "UPDATE planning_runs SET state='succeeded' WHERE build_id=$1",
                params![build],
            )
            .await
            .unwrap();
            sql::execute(
                &mut database.pool.clone(),
                "UPDATE builds SET status='failed' WHERE id=$1",
                params![build],
            )
            .await
            .unwrap();
            scheduler::queue_revisions(&database.state).await.unwrap();
            sql::execute(
                &mut database.pool.clone(),
                "UPDATE planning_runs SET state='succeeded' WHERE build_id=$1",
                params![build],
            )
            .await
            .unwrap();
            sql::execute(
                &mut database.pool.clone(),
                "UPDATE builds SET status='failed' WHERE id=$1",
                params![build],
            )
            .await
            .unwrap();
            scheduler::queue_revisions(&database.state).await.unwrap();
            let count: i32 = sql::scalar(
                &mut database.pool.clone(),
                "SELECT revision_count FROM builds WHERE id=$1",
                params![build],
            )
            .await
            .unwrap();
            assert_eq!(count, 2);
        } else {
            assert_eq!((status, count), ("failed".into(), 0));
        }
    }
    database.close().await;
}

impl Database {
    async fn new() -> Self {
        let url = std::env::var("ARTIFACTORY_TEST_DATABASE_URL")
            .expect("ARTIFACTORY_TEST_DATABASE_URL is required for explicit database tests");
        let admin = connect(&url, Default::default(), PoolSettings::new(2))
            .await
            .unwrap();
        let schema = format!("test_{}", Uuid::new_v4().simple());
        sql::execute(
            &mut admin.clone(),
            &format!("CREATE SCHEMA {schema}"),
            params![],
        )
        .await
        .unwrap();
        let scoped = with_options(&url, &[("search_path", &schema)]).unwrap();
        let pool = connect(
            &scoped,
            artifactory_control_plane::store::models(),
            PoolSettings::new(10),
        )
        .await
        .unwrap();
        artifactory_database::migrate(&pool, &artifactory_control_plane::store::MIGRATIONS)
            .await
            .unwrap();
        let settings = Settings {
            database_url: url,
            http_bind: "127.0.0.1:0".into(),
            grpc_bind: "127.0.0.1:0".into(),
            public_url: "http://localhost:3000".into(),
            insecure_dev: true,
            registration_enabled: true,
            pki_dir: ".local/pki".into(),
            data_dir: std::env::temp_dir(),
            master_key: [1; 32],
            s3_endpoint: "http://localhost:19000".into(),
            s3_public_endpoint: "http://localhost:19000".into(),
            s3_bucket: "test".into(),
            s3_region: "us-east-1".into(),
            s3_access_key: "test".into(),
            s3_secret_key: "test".into(),
            telemetry: None,
        };
        let storage = Arc::new(Storage::new(&settings).unwrap());
        Self {
            state: AppState {
                store: artifactory_control_plane::store::ControlStore::new(pool.clone()),
                settings: Arc::new(settings),
                storage,
                connections: Arc::new(RwLock::new(HashMap::new())),
            },
            pool,
            admin,
            schema,
        }
    }
    async fn close(self) {
        sql::execute(
            &mut self.admin.clone(),
            &format!("DROP SCHEMA {} CASCADE", self.schema),
            params![],
        )
        .await
        .unwrap();
    }
    async fn fixture(&self) -> (Uuid, Uuid, Uuid) {
        let project = Uuid::new_v4();
        let build = Uuid::new_v4();
        let job = Uuid::new_v4();
        let recipe=Recipe::parse("version: 1\nrelease: {enabled: true}\ntargets:\n  app:\n    os: linux\n    arch: amd64\n    steps: [{name: compile, run: make}]\n    artifacts: [dist/app]\n").unwrap();
        sql::execute(&mut self.pool.clone(), "INSERT INTO projects(id,name,repository_url) VALUES($1,'test','https://example.com/repo.git')", params![project]).await.unwrap();
        sql::execute(&mut self.pool.clone(), "INSERT INTO builds(id,project_id,requested_ref,commit_sha,config_snapshot,status,release_version,source_repository_url) VALUES($1,$2,'main',$3,$4,'queued','v1','https://example.com/repo.git')", params![build, project, "0".repeat(40), serde_json::to_value(&recipe).unwrap()]).await.unwrap();
        sql::execute(
            &mut self.pool.clone(),
            "INSERT INTO jobs(id,build_id,target,target_config) VALUES($1,$2,'app',$3)",
            params![
                job,
                build,
                serde_json::to_value(&recipe.targets["app"]).unwrap()
            ],
        )
        .await
        .unwrap();
        (project, build, job)
    }
    async fn node(&self) -> Uuid {
        let node = Uuid::new_v4();
        let session = Uuid::new_v4();
        let (sender, _receiver) = mpsc::channel::<Result<Command, tonic::Status>>(128);
        sql::execute(&mut self.pool.clone(), "INSERT INTO nodes(id,name,os,arch,pool_id,capabilities,certificate_hash,certificate_expires_at,active_session,last_seen) VALUES($1,'test','linux','amd64','00000000-0000-0000-0000-000000000001','[\"git\",\"bash\"]',$2,now()+interval '1 day',$3,now())", params![node, node.to_string(), session]).await.unwrap();
        self.state
            .connections
            .write()
            .await
            .insert(node, Connection { session, sender });
        node
    }
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn build_summary_cannot_overwrite_retry_or_planning_committed_while_waiting() {
    let database = Database::new().await;
    for next in ["queued", "planning"] {
        let (_, build, job) = database.fixture().await;
        sql::execute(
            &mut database.pool.clone(),
            "UPDATE jobs SET status='failed' WHERE id=$1",
            params![job],
        )
        .await
        .unwrap();
        sql::execute(
            &mut database.pool.clone(),
            "UPDATE builds SET status='failed',finished_at=now() WHERE id=$1",
            params![build],
        )
        .await
        .unwrap();

        let mut writer_db = database.pool.clone();
        let mut writer = writer_db.transaction().await.unwrap();
        let backend: i32 = sql::scalar(&mut writer, "SELECT pg_backend_pid()", params![])
            .await
            .unwrap();
        sql::execute(
            &mut writer,
            "SELECT id FROM builds WHERE id=$1 FOR UPDATE",
            params![build],
        )
        .await
        .unwrap();
        let store = database.state.store.clone();
        let summary = tokio::spawn(async move { store.refresh_builds().await });
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let blocked: bool = sql::scalar(&mut database.pool.clone(), "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)) AND query LIKE 'WITH summaries%')", params![backend]).await.unwrap();
                if blocked { break; }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        // The summary already read the old failed jobs. Commit the same atomic
        // job/build transition used by a retry while it waits for the build.
        sql::execute(
            &mut writer,
            "UPDATE jobs SET status='queued' WHERE id=$1",
            params![job],
        )
        .await
        .unwrap();
        sql::execute(
            &mut writer,
            "UPDATE builds SET status=$2,finished_at=NULL WHERE id=$1",
            params![build, next],
        )
        .await
        .unwrap();
        writer.commit().await.unwrap();
        summary.await.unwrap().unwrap();
        let current: (String, Option<chrono::DateTime<chrono::Utc>>) = sql::tuple(
            &mut database.pool.clone(),
            "SELECT status,finished_at FROM builds WHERE id=$1",
            params![build],
        )
        .await
        .unwrap();
        assert_eq!(
            current,
            (next.into(), None),
            "a stale summary must not replace a newly committed state"
        );
        database.state.store.refresh_builds().await.unwrap();
        let status: String = sql::scalar(
            &mut database.pool.clone(),
            "SELECT status FROM builds WHERE id=$1",
            params![build],
        )
        .await
        .unwrap();
        assert_eq!(status, next);
    }
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn concurrent_schedulers_do_not_double_assign_or_exceed_slots() {
    let database = Database::new().await;
    let (_, _, job) = database.fixture().await;
    sql::execute(&mut database.pool.clone(), "INSERT INTO jobs(id,build_id,target,target_config) SELECT $1,build_id,'second',target_config FROM jobs WHERE id=$2", params![Uuid::new_v4(), job]).await.unwrap();
    database.node().await;
    let (first, second) = tokio::join!(
        scheduler::tick(&database.state),
        scheduler::tick(&database.state)
    );
    first.unwrap();
    second.unwrap();
    let attempts: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM attempts WHERE job_id=$1",
        params![job],
    )
    .await
    .unwrap();
    assert_eq!(attempts, 1);
    let total: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM attempts",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(total, 1);
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn queued_signing_authorization_is_rechecked() {
    let database = Database::new().await;
    let (project, _, _) = database.fixture().await;
    let node = database.node().await;
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE jobs SET target_config=jsonb_set(target_config,'{signing_profiles}','[\"apple\"]')",
        params![],
    )
    .await
    .unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE nodes SET signing_profiles='{apple}' WHERE id=$1",
        params![node],
    )
    .await
    .unwrap();
    scheduler::tick(&database.state).await.unwrap();
    let count: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM attempts",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(count, 0);
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE projects SET signing_profiles='{apple}' WHERE id=$1",
        params![project],
    )
    .await
    .unwrap();
    scheduler::tick(&database.state).await.unwrap();
    let count: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM attempts",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(count, 1);
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn scoped_retry_cannot_enable_publication_and_source_is_snapshotted() {
    let database = Database::new().await;
    let (project, _, job) = database.fixture().await;
    let user = Uuid::new_v4();
    sql::execute(&mut database.pool.clone(), "INSERT INTO users(id,username,password_hash,role) VALUES($1,'builder','unused','operator')", params![user]).await.unwrap();
    sql::execute(&mut database.pool.clone(), "INSERT INTO api_tokens(id,user_id,name,token_hash,project_ids,scopes,expires_at) VALUES($1,$2,'build-only',$3,$4,'{read,build}',now()+interval '1 hour')", params![Uuid::new_v4(), user, crypto::hash("builder"), vec![project]]).await.unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE jobs SET status='failed' WHERE id=$1",
        params![job],
    )
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
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE projects SET repository_url='https://example.com/changed.git' WHERE id=$1",
        params![project],
    )
    .await
    .unwrap();
    let repository: String = sql::scalar(
        &mut database.pool.clone(),
        "SELECT source_repository_url FROM builds WHERE id=$1",
        params![build],
    )
    .await
    .unwrap();
    assert_eq!(repository, "https://example.com/repo.git");
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn incomplete_artifacts_never_make_a_release_visible() {
    let database = Database::new().await;
    let (_, build, _) = database.fixture().await;
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE builds SET status='succeeded' WHERE id=$1",
        params![build],
    )
    .await
    .unwrap();
    assert!(
        scheduler::publish_build(&database.state, build)
            .await
            .is_err()
    );
    let count: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM releases",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(count, 0);
    let count: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM publish_attempts WHERE status='failed'",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(count, 1);
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn expired_started_tasks_are_lost_and_unstarted_tasks_requeue() {
    let database = Database::new().await;
    let (_, _, job) = database.fixture().await;
    database.node().await;
    scheduler::tick(&database.state).await.unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE attempts SET lease_expires_at=now()-interval '1 second'",
        params![],
    )
    .await
    .unwrap();
    scheduler::expire(&database.state).await.unwrap();
    let state: String = sql::scalar(
        &mut database.pool.clone(),
        "SELECT status FROM jobs WHERE id=$1",
        params![job],
    )
    .await
    .unwrap();
    assert_eq!(state, "queued");
    scheduler::tick(&database.state).await.unwrap();
    sql::execute(&mut database.pool.clone(), "UPDATE attempts SET status='running',started_at=now(),lease_expires_at=now()-interval '1 second' WHERE status='leased'", params![]).await.unwrap();
    scheduler::expire(&database.state).await.unwrap();
    let state: String = sql::scalar(
        &mut database.pool.clone(),
        "SELECT status FROM jobs WHERE id=$1",
        params![job],
    )
    .await
    .unwrap();
    assert_eq!(state, "lost");
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn publication_is_atomic_immutable_and_idempotent() {
    let database = Database::new().await;
    let (_, build, job) = database.fixture().await;
    let node = database.node().await;
    let attempt = Uuid::new_v4();
    let artifact = Uuid::new_v4();
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE jobs SET status='succeeded'",
        params![],
    )
    .await
    .unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE builds SET status='succeeded'",
        params![],
    )
    .await
    .unwrap();
    sql::execute(&mut database.pool.clone(), "INSERT INTO attempts(id,job_id,node_id,lease_token,lease_expires_at,status,command_id) VALUES($1,$2,$3,'test',now(),'succeeded',$4)", params![attempt, job, node, Uuid::new_v4()]).await.unwrap();
    sql::execute(&mut database.pool.clone(), "INSERT INTO uploads(id,attempt_id,path,object_key,expected_size,expected_sha256,expires_at) VALUES($1,$2,'dist/app',$3,5,$4,now())", params![artifact, attempt, format!("upload/{artifact}"), "a".repeat(64)]).await.unwrap();
    sql::execute(&mut database.pool.clone(), "INSERT INTO artifacts(id,attempt_id,path,object_key,size,sha256) VALUES($1,$2,'dist/app',$3,5,$4)", params![artifact, attempt, format!("artifact/{artifact}"), "a".repeat(64)]).await.unwrap();
    let (first, second) = tokio::join!(
        scheduler::publish_build(&database.state, build),
        scheduler::publish_build(&database.state, build)
    );
    assert_eq!(first.unwrap(), second.unwrap());
    let count: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM releases",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(count, 1);
    let count: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM release_artifacts",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(count, 1);
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn real_http_authorization_rejects_csrf_and_scope_escalation() {
    let database = Database::new().await;
    let (project, _, _) = database.fixture().await;
    let user = Uuid::new_v4();
    sql::execute(&mut database.pool.clone(), "INSERT INTO users(id,username,password_hash,role) VALUES($1,'operator','unused','operator')", params![user]).await.unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "INSERT INTO sessions VALUES($1,$2,now()+interval '1 hour')",
        params![crypto::hash("session"), user],
    )
    .await
    .unwrap();
    sql::execute(&mut database.pool.clone(), "INSERT INTO api_tokens(id,user_id,name,token_hash,project_ids,scopes,expires_at) VALUES($1,$2,'scoped',$3,$4,'{read}',now()+interval '1 hour')", params![Uuid::new_v4(), user, crypto::hash("api-token"), vec![project]]).await.unwrap();
    let router = api::router(database.state.clone());
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/projects/{project}/builds"))
        .header("content-type", "application/json")
        .header("cookie", "artifactory_session=session")
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
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn node_revocation_audit_records_effective_irreversible_state() {
    let database = Database::new().await;
    let node = database.node().await;
    let user = Uuid::new_v4();
    sql::execute(
        &mut database.pool.clone(),
        "INSERT INTO users(id,username,password_hash,role) VALUES($1,'admin','unused','admin')",
        params![user],
    )
    .await
    .unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "INSERT INTO sessions VALUES($1,$2,now()+interval '1 hour')",
        params![crypto::hash("admin-session"), user],
    )
    .await
    .unwrap();
    let router = api::router(database.state.clone());
    for revoked in [true, false] {
        let request = Request::builder()
            .method("PATCH")
            .uri(format!("/api/v1/nodes/{node}"))
            .header("content-type", "application/json")
            .header("cookie", "artifactory_session=admin-session")
            .header("x-artifactory-csrf", "1")
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
    let revoked: bool = sql::scalar(
        &mut database.pool.clone(),
        "SELECT revoked FROM nodes WHERE id=$1",
        params![node],
    )
    .await
    .unwrap();
    let states: Vec<bool> = sql::scalars(&mut database.pool.clone(), "SELECT (details->>'revoked')::boolean FROM audit_events WHERE action='node_updated' AND resource_id=$1 ORDER BY id", params![node.to_string()])
    .await
    .unwrap();
    assert!(revoked);
    assert_eq!(states, vec![true, true]);
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn ota_uses_node_lock_and_pool_limit_and_never_changes_admin_drain() {
    let database = Database::new().await;
    let (_, _, _job) = database.fixture().await;
    let first = database.node().await;
    let second = database.node().await;
    let release = Uuid::new_v4();
    sql::execute(&mut database.pool.clone(), "INSERT INTO emissary_releases(id,version,manifest,signature,manifest_hash) VALUES($1,'v-next',$2,'test','hash')", params![release, serde_json::json!({"assets":[{"os":"linux","arch":"amd64","name":"artifactory-emissary-linux-amd64"}]}).to_string()]).await.unwrap();
    sql::execute(&mut database.pool.clone(), "UPDATE nodes SET capabilities=capabilities || '[\"emissary-ota-v1\"]'::jsonb,emissary_current_version='v-old',emissary_launcher_version='v-old',draining=true", params![]).await.unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE node_pools SET emissary_target_release=$1",
        params![release],
    )
    .await
    .unwrap();
    let (a, b) = tokio::join!(
        scheduler::tick(&database.state),
        scheduler::tick(&database.state)
    );
    a.unwrap();
    b.unwrap();
    let count: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM emissary_updates WHERE status='offered'",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(
        count, 1,
        "pool concurrency must be one across racing schedulers"
    );
    let updates: Vec<Uuid> = sql::scalars(
        &mut database.pool.clone(),
        "SELECT node_id FROM emissary_updates",
        params![],
    )
    .await
    .unwrap();
    assert!(updates[0] == first || updates[0] == second);
    let attempts: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM attempts",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(attempts, 0);
    let drained: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM nodes WHERE draining",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(drained, 2, "OTA must preserve administrator drain");
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn ota_management_rejects_operator_and_project_tokens() {
    let database = Database::new().await;
    let (project, _, _) = database.fixture().await;
    let user = Uuid::new_v4();
    sql::execute(&mut database.pool.clone(), "INSERT INTO users(id,username,password_hash,role) VALUES($1,'ota-operator','unused','operator')", params![user]).await.unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "INSERT INTO sessions VALUES($1,$2,now()+interval '1 hour')",
        params![crypto::hash("ota-session"), user],
    )
    .await
    .unwrap();
    sql::execute(&mut database.pool.clone(), "INSERT INTO api_tokens(id,user_id,name,token_hash,project_ids,scopes,expires_at) VALUES($1,$2,'scoped',$3,$4,'{read,build,release}',now()+interval '1 hour')", params![Uuid::new_v4(), user, crypto::hash("ota-token"), vec![project]]).await.unwrap();
    let router = api::router(database.state.clone());
    for (method, path, body) in [
        (
            "POST",
            "/api/v1/emissary-releases".to_string(),
            r#"{"tag":"v-next"}"#.to_string(),
        ),
        (
            "PATCH",
            format!("/api/v1/nodes/{}/emissary-update", Uuid::new_v4()),
            r#"{"release_id":null}"#.to_string(),
        ),
        (
            "PATCH",
            "/api/v1/pools/00000000-0000-0000-0000-000000000001/emissary-update".into(),
            r#"{"release_id":null}"#.into(),
        ),
        (
            "POST",
            format!("/api/v1/emissary-updates/{}/retry", Uuid::new_v4()),
            "{}".into(),
        ),
        (
            "POST",
            format!("/api/v1/emissary-updates/{}/cancel", Uuid::new_v4()),
            "{}".into(),
        ),
    ] {
        for token in [false, true] {
            let mut request = Request::builder()
                .method(method)
                .uri(&path)
                .header("content-type", "application/json")
                .header("origin", "http://localhost:3000")
                .header("x-artifactory-csrf", "1");
            request = if token {
                request.header("authorization", "Bearer ota-token")
            } else {
                request.header("cookie", "artifactory_session=ota-session")
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
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn ota_failed_activation_pauses_pool_and_fences_stale_sessions() {
    let database = Database::new().await;
    let node = database.node().await;
    let release = Uuid::new_v4();
    sql::execute(&mut database.pool.clone(), "INSERT INTO emissary_releases(id,version,manifest,signature,manifest_hash) VALUES($1,'v-next',$2,'test','hash')", params![release, serde_json::json!({"assets":[{"os":"linux","arch":"amd64","name":"artifactory-emissary-linux-amd64"}]}).to_string()]).await.unwrap();
    sql::execute(&mut database.pool.clone(), "UPDATE nodes SET capabilities=capabilities || '[\"emissary-ota-v1\"]'::jsonb,emissary_current_version='v-old',emissary_target_release=$1,draining=true WHERE id=$2", params![release, node]).await.unwrap();
    scheduler::tick(&database.state).await.unwrap();
    let id: Uuid = sql::scalar(
        &mut database.pool.clone(),
        "SELECT id FROM emissary_updates WHERE node_id=$1",
        params![node],
    )
    .await
    .unwrap();
    let session = database.state.connections.read().await[&node].session;
    let payload = serde_json::json!({"diagnostics":{"emissary_version":"v-old"},"ota":{"update_id":id,"phase":"rolled_back","launcher_version":"v-old","error":"candidate crashed"}});
    assert!(
        artifactory_control_plane::ota::report(&database.state, node, Uuid::new_v4(), &payload)
            .await
            .is_err()
    );
    let status: String = sql::scalar(
        &mut database.pool.clone(),
        "SELECT status FROM emissary_updates WHERE id=$1",
        params![id],
    )
    .await
    .unwrap();
    assert_eq!(status, "offered");
    let preparing = serde_json::json!({"diagnostics":{"emissary_version":"v-old"},"ota":{"update_id":id,"phase":"activating","activation_pending":true,"launcher_version":"v-old"}});
    assert!(
        artifactory_control_plane::ota::report(&database.state, node, session, &preparing)
            .await
            .unwrap(),
        "old runtime must receive acknowledgment before yielding to a candidate"
    );
    assert!(
        artifactory_control_plane::ota::report(&database.state, node, session, &payload)
            .await
            .unwrap()
    );
    let flags:(bool,bool,bool)=sql::tuple(&mut database.pool.clone(), "SELECT n.draining,n.ota_draining,p.emissary_updates_paused FROM nodes n JOIN node_pools p ON p.id=n.pool_id WHERE n.id=$1", params![node]).await.unwrap();
    assert_eq!(flags, (true, false, true));
    scheduler::tick(&database.state).await.unwrap();
    let count: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM emissary_updates",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(count, 1, "failed release must never loop automatically");
    // Revocation and certificate expiry are enforced within the status transaction.
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE nodes SET revoked=true WHERE id=$1",
        params![node],
    )
    .await
    .unwrap();
    assert!(
        artifactory_control_plane::ota::report(&database.state, node, session, &payload)
            .await
            .is_err()
    );
    sql::execute(&mut database.pool.clone(), "UPDATE nodes SET revoked=false,certificate_expires_at=now()-interval '1 minute' WHERE id=$1", params![node]).await.unwrap();
    assert!(
        artifactory_control_plane::ota::report(&database.state, node, session, &payload)
            .await
            .is_err()
    );
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn ota_retry_after_pool_move_respects_new_pool_capacity() {
    let database = Database::new().await;
    let node = database.node().await;
    let other = database.node().await;
    let release = Uuid::new_v4();
    let prior = Uuid::new_v4();
    let new_pool = Uuid::new_v4();
    sql::execute(&mut database.pool.clone(), "INSERT INTO emissary_releases(id,version,manifest,signature,manifest_hash) VALUES($1,'v-next','{}','test','hash')", params![release]).await.unwrap();
    sql::execute(&mut database.pool.clone(), "INSERT INTO emissary_updates(id,node_id,pool_id,release_id,status,finished_at) VALUES($1,$2,'00000000-0000-0000-0000-000000000001',$3,'failed',now())", params![prior, node, release]).await.unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "INSERT INTO node_pools(id,name) VALUES($1,'new pool')",
        params![new_pool],
    )
    .await
    .unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE nodes SET pool_id=$1",
        params![new_pool],
    )
    .await
    .unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "INSERT INTO emissary_updates(id,node_id,pool_id,release_id) VALUES($1,$2,$3,$4)",
        params![Uuid::new_v4(), other, new_pool, release],
    )
    .await
    .unwrap();
    let user = Uuid::new_v4();
    sql::execute(
        &mut database.pool.clone(),
        "INSERT INTO users(id,username,password_hash,role) VALUES($1,'admin','unused','admin')",
        params![user],
    )
    .await
    .unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "INSERT INTO sessions VALUES($1,$2,now()+interval '1 hour')",
        params![crypto::hash("admin-session"), user],
    )
    .await
    .unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/emissary-updates/{prior}/retry"))
        .header("content-type", "application/json")
        .header("cookie", "artifactory_session=admin-session")
        .header("x-artifactory-csrf", "1")
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
