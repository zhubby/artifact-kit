use artifactory_runtime_protocol::{crypto, objects::Objects, service::Client};
use artifactory_source_core::{self as core, LfsObject, Repository, Snapshot};
use artifactory_source_service::service::{AppState, persist, restore};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::{str::FromStr, sync::Arc};
use uuid::Uuid;
#[tokio::test]
#[ignore = "requires isolated PostgreSQL and private S3"]
async fn pvc_loss_restores_exact_commit_tag_and_lfs_without_upstream_and_tools_stay_bounded() {
    let url = std::env::var("ARTIFACTORY_TEST_DATABASE_URL").unwrap();
    let admin = PgPoolOptions::new().connect(&url).await.unwrap();
    let schema = format!("src_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .connect_with(
            PgConnectOptions::from_str(&url)
                .unwrap()
                .options([("search_path", schema.as_str())]),
        )
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let state = AppState {
        core: core::AppState {
            store: core::store::SourceStore::new(pool.clone()),
            settings: Arc::new(core::Settings {
                data_dir: directory.path().into(),
                master_key: [3; 32],
            }),
        },
        objects: Objects::from_env().unwrap(),
        control: Client::new("https://control.invalid".into(), "x".repeat(64)).unwrap(),
        runtime: Client::new("https://runtime.invalid".into(), "x".repeat(64)).unwrap(),
        origin: "https://source.invalid".into(),
    };
    use tower::ServiceExt;
    let response = artifactory_source_service::service::router(state.clone(), "x".repeat(64))
        .oneshot(
            axum::http::Request::builder()
                .uri(format!(
                    "/api/v1/source/{}/{}/git/info/refs?service=git-upload-pack",
                    Uuid::new_v4(),
                    Uuid::new_v4()
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert!(
        response.headers().contains_key("www-authenticate"),
        "Git needs a Basic challenge before using the task-scoped askpass token"
    );
    state.objects.initialize().await.unwrap();
    let project = Uuid::new_v4();
    let repository = Uuid::new_v4();
    let key = crypto::hash(repository.to_string());
    sqlx::query("INSERT INTO source_repositories(id,project_id,repository_url,cache_key) VALUES($1,$2,'https://unreachable.invalid/source.git',$3)").bind(repository).bind(project).bind(key).execute(&pool).await.unwrap();
    let repo = core::repository_path(&state, repository).await.unwrap();
    tokio::fs::create_dir_all(&repo).await.unwrap();
    core::git(&repo, &["init", "-b", "main"]).await.unwrap();
    core::git(&repo, &["config", "user.name", "Source fixture"])
        .await
        .unwrap();
    core::git(&repo, &["config", "user.email", "source@localhost"])
        .await
        .unwrap();
    core::git(&repo, &["config", "commit.gpgsign", "false"])
        .await
        .unwrap();
    let payload = b"immutable LFS data\n";
    let oid = crypto::hash(payload);
    tokio::fs::write(repo.join("README.md"), "original source\n")
        .await
        .unwrap();
    tokio::fs::write(
        repo.join("payload.dat"),
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
            payload.len()
        ),
    )
    .await
    .unwrap();
    core::git(&repo, &["add", "."]).await.unwrap();
    core::git(&repo, &["commit", "-m", "original"])
        .await
        .unwrap();
    core::git(&repo, &["tag", "-a", "frozen-v1", "-m", "frozen tag"])
        .await
        .unwrap();
    let commit = String::from_utf8(core::git(&repo, &["rev-parse", "HEAD"]).await.unwrap())
        .unwrap()
        .trim()
        .to_string();
    let tag = String::from_utf8(core::git(&repo, &["rev-parse", "frozen-v1"]).await.unwrap())
        .unwrap()
        .trim()
        .to_string();
    // Convert the fixture into the same bare cache layout as a fetch.
    let bare = repo.with_extension("bare");
    core::git(
        directory.path(),
        &[
            "clone",
            "--bare",
            repo.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    )
    .await
    .unwrap();
    tokio::fs::remove_dir_all(&repo).await.unwrap();
    tokio::fs::rename(bare, &repo).await.unwrap();
    let lfs = core::lfs_path(&repo, &oid).unwrap();
    tokio::fs::create_dir_all(lfs.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(lfs, payload).await.unwrap();
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
                oid: oid.clone(),
                size: payload.len() as u64,
            }],
        }],
    };
    sqlx::query("INSERT INTO source_snapshots(id,project_id,root_repository_id,commit_sha,manifest,digest) VALUES($1,$2,$3,$4,$5,$6)").bind(snapshot.id).bind(project).bind(repository).bind(commit).bind(serde_json::to_value(&snapshot).unwrap()).bind(crypto::hash(serde_json::to_vec(&snapshot).unwrap())).execute(&pool).await.unwrap();
    core::ensure_available(&state, &snapshot).await.unwrap();
    persist(&state, &snapshot).await.unwrap();
    tokio::fs::remove_dir_all(&repo).await.unwrap();
    restore(&state, &snapshot).await.unwrap();
    assert_eq!(
        core::inspect_tool(
            &state,
            snapshot.id,
            "read_file",
            &json!({"path":"README.md"})
        )
        .await
        .unwrap()["content"],
        "original source\n"
    );
    assert_eq!(
        core::inspect_tool(
            &state,
            snapshot.id,
            "read_file",
            &json!({"path":"payload.dat"})
        )
        .await
        .unwrap()["content"],
        String::from_utf8_lossy(payload).as_ref()
    );
    assert_eq!(
        core::git(&repo, &["cat-file", "-t", &tag]).await.unwrap(),
        b"tag\n"
    );
    assert!(
        core::inspect_tool(
            &state,
            snapshot.id,
            "read_file",
            &json!({"path":"../outside"})
        )
        .await
        .is_err()
    );
    tokio::fs::write(core::lfs_path(&repo, &oid).unwrap(), b"tampered bytes")
        .await
        .unwrap();
    assert!(restore(&state, &snapshot).await.is_err());
}
