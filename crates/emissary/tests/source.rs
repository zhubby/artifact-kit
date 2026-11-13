use artifactory_emissary::{
    config::Config,
    protocol::{Assignment, SourceDescriptor, SourceRepository},
    source,
    worker::Emitter,
};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    routing::any,
};
use std::{
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

fn git(path: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-C",
            path.to_str().unwrap(),
            "-c",
            "user.name=Source test",
            "-c",
            "user.email=test@localhost",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", source::NULL)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
#[derive(Clone)]
struct Fixture {
    root: std::path::PathBuf,
    requests: Arc<AtomicUsize>,
}
async fn backend(
    State(fixture): State<Fixture>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    use std::io::Write;
    fixture.requests.fetch_add(1, Ordering::SeqCst);
    let output = tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new("git");
        command
            .arg("http-backend")
            .env("GIT_PROJECT_ROOT", fixture.root)
            .env("GIT_HTTP_EXPORT_ALL", "1")
            .env(
                "PATH_INFO",
                format!("/repo.git/{}", uri.path().trim_start_matches("/git/")),
            )
            .env("REQUEST_METHOD", method.as_str())
            .env("QUERY_STRING", uri.query().unwrap_or(""))
            .env(
                "CONTENT_TYPE",
                headers
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or(""),
            )
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(&body).unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    assert!(output.status.success());
    let split = output
        .stdout
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap();
    let mut headers = HeaderMap::new();
    for line in String::from_utf8_lossy(&output.stdout[..split]).lines() {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-type")
        {
            headers.insert("content-type", value.trim().parse().unwrap());
        }
    }
    (StatusCode::OK, headers, output.stdout[split + 4..].to_vec())
}
#[tokio::test]
async fn checkout_reuses_objects_and_isolates_attempts() {
    let root = tempfile::tempdir().unwrap();
    let upstream = root.path().join("repo.git");
    fs::create_dir(&upstream).unwrap();
    git(&upstream, &["init", "-b", "source"]);
    fs::write(upstream.join("main.c"), "int main(void) { return 0; }\n").unwrap();
    git(&upstream, &["add", "."]);
    git(&upstream, &["commit", "-m", "immutable fixture"]);
    let commit = git(&upstream, &["rev-parse", "HEAD"]);
    git(&upstream, &["tag", "-a", "v1", "-m", "frozen tag"]);
    let tag = git(&upstream, &["rev-parse", "refs/tags/v1"]);
    let requests = Arc::new(AtomicUsize::new(0));
    let app = Router::new().fallback(any(backend)).with_state(Fixture {
        root: root.path().into(),
        requests: requests.clone(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cfg = Config {
        root: root.path().join("managed"),
        allow_http_uploads: true,
        ..Default::default()
    };
    let assignment = Assignment {
        commit_sha: commit.clone(),
        source: Some(SourceDescriptor {
            snapshot_id: uuid::Uuid::new_v4().to_string(),
            access_token: "a".repeat(64),
            repositories: vec![SourceRepository {
                repository_id: uuid::Uuid::new_v4().to_string(),
                cache_id: uuid::Uuid::new_v4().to_string(),
                commit_sha: commit,
                git_url: format!("http://{address}/git"),
                tags: std::collections::HashMap::from([("v1".into(), tag)]),
                ..Default::default()
            }],
        }),
        ..Default::default()
    };
    let (emit, mut receive) = Emitter::channel();
    tokio::spawn(async move { while receive.recv().await.is_some() {} });
    let cancel = CancellationToken::new();
    let first = root.path().join("first");
    let second = root.path().join("second");
    source::checkout(&cfg, &assignment, root.path(), &first, &cancel, &emit)
        .await
        .unwrap();
    let initial = requests.load(Ordering::SeqCst);
    assert!(initial > 0);
    source::checkout(&cfg, &assignment, root.path(), &second, &cancel, &emit)
        .await
        .unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), initial);
    assert_eq!(git(&second, &["describe", "--tags"]), "v1");
    fs::write(first.join("main.c"), "changed\n").unwrap();
    assert!(
        source::verify(&cfg, &assignment, &first, &cancel)
            .await
            .unwrap_err()
            .to_string()
            .contains("source_modified")
    );
    source::verify(&cfg, &assignment, &second, &cancel)
        .await
        .unwrap();
    git(&first, &["reset", "--hard", "HEAD"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(first.join("main.c"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            source::verify(&cfg, &assignment, &first, &cancel)
                .await
                .is_err()
        );
    }
    fs::write(second.join("generated.tmp"), "build output").unwrap();
    source::verify(&cfg, &assignment, &second, &cancel)
        .await
        .unwrap();
    for relative in ["../x", "a/../x", ".git/x", "/x", "C:/x", "a\\x"] {
        assert!(source::managed_path(&second, relative).is_err());
    }
    server.abort();
}
#[test]
fn build_environment_and_lfs_files_are_independent() {
    let env = source::build_environment();
    for key in [
        "ANTHROPIC_API_KEY",
        "ARTIFACTORY_MASTER_KEY",
        "ARTIFACTORY_ENROLLMENT_TOKEN",
    ] {
        assert!(!env.contains_key(std::ffi::OsStr::new(key)));
    }
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let attempt = root.path().join("attempt");
    fs::create_dir(&cache).unwrap();
    fs::write(cache.join("object"), "original").unwrap();
    source::copy_lfs(&cache, &attempt).unwrap();
    fs::write(attempt.join("object"), "changed").unwrap();
    assert_eq!(fs::read(cache.join("object")).unwrap(), b"original");
}
