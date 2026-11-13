use super::*;
use artifactory_control_plane::source_cache;

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL and git"]
async fn independent_source_serves_old_snapshots_to_two_empty_control_directories() {
    let database = Database::new().await;
    let (project, build, _) = database.fixture().await;
    let root = tempfile::tempdir().unwrap();
    let upstream = tempfile::tempdir().unwrap();
    let repo = upstream.path().join("repo.git");
    tokio::fs::create_dir(&repo).await.unwrap();
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.name", "Source fixture"],
        vec!["config", "user.email", "source@localhost"],
        vec!["config", "commit.gpgsign", "false"],
    ] {
        source_cache::git(&repo, &args).await.unwrap();
    }
    tokio::fs::write(repo.join("main.c"), "original source\n")
        .await
        .unwrap();
    source_cache::git(&repo, &["add", "."]).await.unwrap();
    source_cache::git(&repo, &["commit", "-m", "original"])
        .await
        .unwrap();
    let mut settings = (*database.state.settings).clone();
    settings.data_dir = root.path().join("9");
    tokio::fs::create_dir_all(&settings.data_dir).await.unwrap();
    let source_state = AppState {
        settings: Arc::new(settings),
        ..database.state.clone()
    };
    let repository = Uuid::new_v4();
    let commit = String::from_utf8(
        source_cache::git(&repo, &["rev-parse", "HEAD"])
            .await
            .unwrap(),
    )
    .unwrap()
    .trim()
    .to_owned();
    sql::execute(&mut database.pool.clone(), "INSERT INTO source_repositories(id,project_id,repository_url,cache_key) VALUES($1,$2,'https://example.com/repo.git',$3)", params![repository, project, crypto::hash(repository.to_string())]).await.unwrap();
    let snapshot = source_cache::Snapshot {
        id: Uuid::new_v4(),
        project_id: project,
        commit: commit.clone(),
        repositories: vec![source_cache::Repository {
            id: Uuid::new_v4(),
            repository_id: repository,
            path: String::new(),
            commit: commit.clone(),
            tags: Default::default(),
            lfs: vec![],
        }],
    };
    sql::execute(&mut database.pool.clone(), "INSERT INTO source_snapshots(id,project_id,root_repository_id,commit_sha,manifest,digest) VALUES($1,$2,$3,$4,$5,$6)", params![snapshot.id, project, repository, commit, serde_json::to_value(&snapshot).unwrap(), snapshot.id.to_string()]).await.unwrap();
    let mirror = source_cache::repository_path(&source_state, repository)
        .await
        .unwrap();
    tokio::fs::create_dir_all(mirror.parent().unwrap())
        .await
        .unwrap();
    source_cache::git(
        root.path(),
        &[
            "clone",
            "--mirror",
            repo.to_str().unwrap(),
            mirror.to_str().unwrap(),
        ],
    )
    .await
    .unwrap();
    source_cache::ensure_available(&source_state, &snapshot)
        .await
        .unwrap();
    // This snapshot existed on the original volume before the split.
    tokio::fs::write(repo.join("main.c"), "changed upstream\n")
        .await
        .unwrap();
    source_cache::git(&repo, &["commit", "-am", "move upstream"])
        .await
        .unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE builds SET source_snapshot_id=$2,commit_sha=$3 WHERE id=$1",
        params![build, snapshot.id, &snapshot.commit],
    )
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let storage = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route("/{*key}", axum::routing::put(|| async { StatusCode::OK })),
        )
        .await
        .unwrap();
    });
    let certificates = Certificates::new();
    let mut source =
        load::ProcessReplica::source(&database, &certificates, root.path(), &endpoint, 9).await;
    let http = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(certificates.ca.as_bytes()).unwrap())
        .build()
        .unwrap();
    assert_eq!(
        http.post(format!("{}/internal/local-source", source.http))
            .json(&json!({"operation":"ensure","snapshot_id":snapshot.id}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http.get(format!("{}/api/v1/auth/me", source.http))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    database.node().await;
    scheduler::schedule(&database.state).await.unwrap();
    let attempt: Uuid = sql::scalar(
        &mut database.pool.clone(),
        "SELECT id FROM attempts",
        params![],
    )
    .await
    .unwrap();
    let mut controls = Vec::new();
    for name in ["control-a", "control-b"] {
        let mut settings = (*database.state.settings).clone();
        settings.data_dir = root.path().join(name);
        settings.pki_dir = root.path().join("9");
        settings.local_source_url = Some(source.http.clone());
        tokio::fs::create_dir(&settings.data_dir).await.unwrap();
        let control = AppState {
            settings: Arc::new(settings),
            coordination: Default::default(),
            source_client: Default::default(),
            connections: Default::default(),
            ..database.state.clone()
        };
        source_cache::ensure_available(&control, &snapshot)
            .await
            .unwrap();
        source_cache::validate_evidence(&control, &snapshot, &["main.c".into()])
            .await
            .unwrap();
        let contents = source_cache::inspect_tool(
            &control,
            snapshot.id,
            "read_file",
            &json!({"path":"main.c"}),
        )
        .await
        .unwrap();
        assert!(contents.to_string().contains("original source"));
        let descriptor = source_cache::descriptor(&control, snapshot.id, attempt)
            .await
            .unwrap();
        assert_eq!(descriptor.snapshot_id, snapshot.id.to_string());
        let response = http
            .get(format!(
                "{}/api/v1/source/{}/{}/git/info/refs?service=git-upload-pack",
                source.http, snapshot.id, snapshot.repositories[0].id
            ))
            .bearer_auth(descriptor.access_token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.bytes().await.unwrap();
        assert!(
            bytes
                .windows(snapshot.commit.len())
                .any(|entry| entry == snapshot.commit.as_bytes())
        );
        assert!(
            !control.settings.data_dir.join("sources").exists(),
            "Control must not re-create a local source mirror"
        );
        controls.push(control);
    }
    // Hold the actual Source restoration lock while the assignment worker is
    // preparing a descriptor. Cancellation, expiry and other scheduling must
    // continue on this Control process during the blocked metadata request.
    let control = AppState {
        coordination: database.state.coordination.clone(),
        connections: database.state.connections.clone(),
        ..controls.remove(0)
    };
    let mut cache_lock_db = database.pool.clone();
    let mut cache_lock = cache_lock_db.transaction().await.unwrap();
    sql::execute(
        &mut cache_lock,
        "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
        params![crypto::hash(repository.to_string())],
    )
    .await
    .unwrap();
    let locker: i32 = sql::scalar(&mut cache_lock, "SELECT pg_backend_pid()", params![])
        .await
        .unwrap();
    scheduler::start(control.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let blocked: bool = sql::scalar(&mut database.pool.clone(), "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event_type='Lock' AND $1=ANY(pg_blocking_pids(pid)))", params![locker]).await.unwrap();
            if blocked { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("assignment must be waiting for the Source cache lock");
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE builds SET cancel_requested=true WHERE id=$1",
        params![build],
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let delivered: bool = sql::scalar(&mut database.pool.clone(), "SELECT next_delivery_at>clock_timestamp() FROM device_commands WHERE attempt_id=$1 AND kind='cancel'", params![attempt]).await.unwrap();
            if delivered { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("Source must not block cancellation delivery");
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE attempts SET lease_expires_at=now()-interval '1 second' WHERE id=$1",
        params![attempt],
    )
    .await
    .unwrap();
    let (_, _, other_job) = database.fixture().await;
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let responsive: bool = sql::scalar(&mut database.pool.clone(), "SELECT EXISTS(SELECT 1 FROM attempts WHERE id=$1 AND status='canceled') AND EXISTS(SELECT 1 FROM attempts WHERE job_id=$2 AND status='leased')", params![attempt, other_job]).await.unwrap();
            if responsive { break; }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }).await.expect("Source must not block lease expiry or other scheduling");
    control.coordination.stop();
    cache_lock.commit().await.unwrap();
    source.terminate().await;
    let request = Request::builder()
        .uri("/readyz")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        api::router(database.state.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    storage.abort();
    drop(source);
    database.close().await;
}
