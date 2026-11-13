use super::*;
use serde_json::{Value, json};

fn registration_request(input: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/auth/register")
        .header("content-type", "application/json")
        .header("x-artifactory-csrf", "1")
        .header("origin", "http://localhost:3000")
        .body(Body::from(input.to_string()))
        .unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    let body = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn rejects_invalid_fields_and_csrf_without_consuming_registration_slots() {
    let database = Database::new().await;
    let router = api::router(database.state.clone());
    for input in [
        json!({"username":"","password":"reader-password"}),
        json!({"username":" \t\n ","password":"reader-password"}),
        json!({"username":"reader\u{0}","password":"reader-password"}),
        json!({"username":"a".repeat(101),"password":"reader-password"}),
        json!({"username":"人".repeat(34),"password":"reader-password"}),
        json!({"username":"reader","password":"too-short"}),
        json!({"username":"reader","password":"a".repeat(257)}),
        json!({"username":"reader","password":"密".repeat(86)}),
        json!({"username":"reader","password":"reader-password","role":"admin"}),
        json!({"username":"reader","password":"reader-password","confirmPassword":"reader-password"}),
        json!({"username":"reader"}),
        json!({"username":null,"password":"reader-password"}),
        json!([]),
    ] {
        let response = router
            .clone()
            .oneshot(registration_request(input.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{input}");
        assert!(!response.headers().contains_key("set-cookie"));
    }
    let mut missing_csrf =
        registration_request(json!({"username":"reader","password":"reader-password"}));
    missing_csrf.headers_mut().remove("x-artifactory-csrf");
    assert_eq!(
        router.clone().oneshot(missing_csrf).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let mut cross_origin =
        registration_request(json!({"username":"reader","password":"reader-password"}));
    cross_origin
        .headers_mut()
        .insert("origin", "https://other.example".parse().unwrap());
    assert_eq!(
        router.oneshot(cross_origin).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let counts: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM users),(SELECT count(*) FROM sessions),(SELECT count(*) FROM audit_events)")
        .fetch_one(&database.pool).await.unwrap();
    assert_eq!(counts, (0, 0, 0));
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn config_switch_blocks_registration_but_preserves_login_and_existing_sessions() {
    let mut database = Database::new().await;
    Arc::make_mut(&mut database.state.settings).insecure_dev = false;
    let router = api::router(database.state.clone());
    let config = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(config.status(), StatusCode::OK);
    assert_eq!(config.headers()["cache-control"], "no-store");
    assert_eq!(
        json_body(config).await,
        json!({"registration_enabled":true})
    );
    let response = router
        .oneshot(registration_request(
            json!({"username":"reader","password":"reader-password"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let set_cookie = response.headers()["set-cookie"].to_str().unwrap();
    assert!(set_cookie.ends_with("; Secure"));
    let cookie = set_cookie.split(';').next().unwrap().to_owned();
    Arc::make_mut(&mut database.state.settings).registration_enabled = false;
    let router = api::router(database.state.clone());
    let config = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        json_body(config).await,
        json!({"registration_enabled":false})
    );
    let response = router
        .clone()
        .oneshot(registration_request(
            json!({"username":"another","password":"reader-password"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(!response.headers().contains_key("set-cookie"));
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut login = registration_request(json!({"username":"reader","password":"reader-password"}));
    *login.uri_mut() = "/api/v1/auth/login".parse().unwrap();
    assert_eq!(
        router.oneshot(login).await.unwrap().status(),
        StatusCode::OK
    );
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn concurrent_duplicate_registration_creates_only_one_user_session_and_audit() {
    let database = Database::new().await;
    let router = api::router(database.state.clone());
    let input = json!({"username":"same-reader","password":"reader-password"});
    let (first, second) = tokio::join!(
        router.clone().oneshot(registration_request(input.clone())),
        router.oneshot(registration_request(input)),
    );
    let mut statuses = [
        first.unwrap().status().as_u16(),
        second.unwrap().status().as_u16(),
    ];
    statuses.sort_unstable();
    assert_eq!(statuses, [201, 409]);
    let counts: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM users),(SELECT count(*) FROM sessions),(SELECT count(*) FROM audit_events WHERE action='user_registered')")
        .fetch_one(&database.pool).await.unwrap();
    assert_eq!(counts, (1, 1, 1));
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn usernames_preserve_case_and_password_policy_uses_utf8_bytes() {
    let database = Database::new().await;
    let router = api::router(database.state.clone());
    for (username, password) in [("Reader", "密".repeat(4)), ("reader", "a".repeat(256))] {
        let response = router
            .clone()
            .oneshot(registration_request(
                json!({"username":username,"password":password}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn session_or_audit_failure_rolls_back_registration() {
    for constraint in [
        "ALTER TABLE sessions ADD CONSTRAINT reject_registration CHECK (false)",
        "ALTER TABLE audit_events ADD CONSTRAINT reject_registration CHECK (action <> 'user_registered')",
    ] {
        let database = Database::new().await;
        sqlx::query(constraint)
            .execute(&database.pool)
            .await
            .unwrap();
        let response = api::router(database.state.clone())
            .oneshot(registration_request(
                json!({"username":"reader","password":"reader-password"}),
            ))
            .await
            .unwrap();
        assert!(!response.status().is_success());
        assert!(!response.headers().contains_key("set-cookie"));
        let counts: (i64, i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM users),(SELECT count(*) FROM sessions),(SELECT count(*) FROM audit_events WHERE action='user_registered'),(SELECT count(*) FROM audit_events WHERE action='registration_attempt')")
            .fetch_one(&database.pool).await.unwrap();
        assert_eq!(counts, (0, 0, 0, 1));
        database.close().await;
    }
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn concurrent_registration_limit_is_persistent_and_expires() {
    let database = Database::new().await;
    sqlx::query("INSERT INTO audit_events(actor,action,resource_id,created_at) SELECT 'anonymous','registration_attempt','registration',now()-interval '6 minutes' FROM generate_series(1,3)")
        .execute(&database.pool).await.unwrap();
    sqlx::query("INSERT INTO audit_events(actor,action,resource_id) SELECT 'anonymous','registration_attempt','registration' FROM generate_series(1,9)")
        .execute(&database.pool).await.unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..4 {
        let router = api::router(database.state.clone());
        tasks.spawn(async move {
            router
                .oneshot(registration_request(
                    json!({"username":format!("reader-{index}"),"password":"reader-password"}),
                ))
                .await
                .unwrap()
        });
    }
    let mut created = 0;
    let mut limited = 0;
    while let Some(result) = tasks.join_next().await {
        let response = result.unwrap();
        match response.status() {
            StatusCode::CREATED => created += 1,
            StatusCode::TOO_MANY_REQUESTS => {
                limited += 1;
                let retry_after: u64 = response.headers()["retry-after"]
                    .to_str()
                    .unwrap()
                    .parse()
                    .unwrap();
                assert!((1..=300).contains(&retry_after));
                assert!(!response.headers().contains_key("set-cookie"));
            }
            status => panic!("unexpected registration status {status}"),
        }
    }
    assert_eq!((created, limited), (1, 3));
    let router = api::router(database.state.clone());
    let input = json!({"username":"after-restart","password":"reader-password"});
    assert_eq!(
        router
            .clone()
            .oneshot(registration_request(input.clone()))
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    sqlx::query("UPDATE audit_events SET created_at=now()-interval '6 minutes' WHERE action='registration_attempt'")
        .execute(&database.pool).await.unwrap();
    assert_eq!(
        router
            .oneshot(registration_request(input))
            .await
            .unwrap()
            .status(),
        StatusCode::CREATED
    );
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn registered_viewer_can_read_and_download_but_cannot_write_or_manage() {
    let database = Database::new().await;
    let (project, _, job) = database.fixture().await;
    let node = database.node().await;
    let attempt = Uuid::new_v4();
    let artifact = Uuid::new_v4();
    sqlx::query("INSERT INTO attempts(id,job_id,node_id,lease_token,lease_expires_at,status,command_id) VALUES($1,$2,$3,'test',now(),'succeeded',$4)")
        .bind(attempt).bind(job).bind(node).bind(Uuid::new_v4()).execute(&database.pool).await.unwrap();
    sqlx::query("INSERT INTO uploads(id,attempt_id,path,object_key,expected_size,expected_sha256,expires_at) VALUES($1,$2,'dist/app','staging/test',1,'test',now())")
        .bind(artifact).bind(attempt).execute(&database.pool).await.unwrap();
    sqlx::query("INSERT INTO artifacts(id,attempt_id,path,object_key,size,sha256) VALUES($1,$2,'dist/app','final/test',1,'test')")
        .bind(artifact).bind(attempt).execute(&database.pool).await.unwrap();
    let router = api::router(database.state.clone());
    let response = router
        .clone()
        .oneshot(registration_request(
            json!({"username":"reader","password":"reader-password"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    for path in ["/projects", "/builds", "/artifacts", "/nodes", "/overview"] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1{path}"))
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/artifacts/{artifact}/download"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert!(
        response.headers()["location"]
            .to_str()
            .unwrap()
            .contains("final/test")
    );
    for (path, input) in [
        (
            "/projects".to_owned(),
            json!({"name":"forbidden","repository_url":"https://example.com/repo.git"}),
        ),
        (
            format!("/projects/{project}/builds"),
            json!({"ref":"main","goal":"Build the application"}),
        ),
        (
            "/users".to_owned(),
            json!({"username":"elevated","password":"reader-password","role":"admin"}),
        ),
        (
            "/credentials".to_owned(),
            json!({"name":"secret","kind":"https","username":"reader","secret":"private","known_hosts":""}),
        ),
    ] {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/api/v1{path}"))
            .header("cookie", &cookie)
            .header("content-type", "application/json")
            .header("x-artifactory-csrf", "1")
            .body(Body::from(input.to_string()))
            .unwrap();
        assert_eq!(
            router.clone().oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    for path in ["/users", "/credentials"] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1{path}"))
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    database.close().await;
}
