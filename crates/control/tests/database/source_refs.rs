use super::*;
use serde_json::{Value, json};

async fn read_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap()
}

async fn token(database: &Database, role: &str, projects: &[Uuid], scopes: &[&str]) -> String {
    let user = Uuid::new_v4();
    let secret = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO users(id,username,password_hash,role) VALUES($1,$2,'unused',$3)")
        .bind(user)
        .bind(user.to_string())
        .bind(role)
        .execute(&database.state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO api_tokens(id,user_id,name,token_hash,project_ids,scopes,expires_at) VALUES($1,$2,'refs-test',$3,$4,$5,now()+interval '1 hour')")
        .bind(Uuid::new_v4()).bind(user).bind(crypto::hash(&secret)).bind(projects)
        .bind(scopes).execute(&database.state.pool).await.unwrap();
    secret
}

async fn request(
    database: &Database,
    project: Uuid,
    secret: Option<&str>,
) -> axum::response::Response {
    let mut builder = Request::builder().uri(format!("/api/v1/projects/{project}/refs"));
    if let Some(secret) = secret {
        builder = builder.header("authorization", format!("Bearer {secret}"));
    }
    api::router(database.state.clone())
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn references_enforce_read_scope_project_scope_and_not_found() {
    let database = Database::new().await;
    let project = Uuid::new_v4();
    sqlx::query("INSERT INTO projects(id,name,repository_url) VALUES($1,'refs','https://127.0.0.1:1/repo.git')")
        .bind(project).execute(&database.state.pool).await.unwrap();
    assert_eq!(
        request(&database, project, None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    for (projects, scopes) in [
        (vec![Uuid::new_v4()], vec!["read"]),
        (vec![project], vec!["build"]),
    ] {
        let secret = token(&database, "operator", &projects, &scopes).await;
        assert_eq!(
            request(&database, project, Some(&secret)).await.status(),
            StatusCode::FORBIDDEN
        );
    }
    let missing = Uuid::new_v4();
    let secret = token(&database, "viewer", &[project, missing], &["read"]).await;
    assert_eq!(
        request(&database, missing, Some(&secret)).await.status(),
        StatusCode::NOT_FOUND
    );
    let response = request(&database, project, Some(&secret)).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = read_body(response).await.to_string();
    assert!(body.contains("check repository access"));
    assert!(!body.contains("127.0.0.1"));
    let builds: i64 = sqlx::query_scalar("SELECT count(*) FROM builds")
        .fetch_one(&database.state.pool)
        .await
        .unwrap();
    assert_eq!(builds, 0);
    database.close().await;
}

#[test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL, openssl and sshd"]
fn repository_refs_use_only_project_credentials_and_host_trust() {
    let directory = tempfile::Builder::new()
        .prefix("source refs with spaces ")
        .tempdir()
        .unwrap();
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/source_refs.py"
        ))
        .arg(std::env::current_exe().unwrap())
        .arg(directory.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "reference fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn repository_refs_probe_helper() {
    let Ok(path) = std::env::var("ARTIFACTORY_TEST_REFS_FIXTURE") else {
        return;
    };
    let fixture: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let database = Database::new().await;
    for entry in fixture["cases"].as_array().unwrap() {
        let credential = (entry["kind"] != "none").then(Uuid::new_v4);
        let project = Uuid::new_v4();
        let secret = if entry["kind"] == "ssh" {
            std::fs::read_to_string(entry["key_path"].as_str().unwrap()).unwrap()
        } else {
            entry["password"]
                .as_str()
                .unwrap_or("no-project-credential")
                .to_owned()
        };
        if let Some(credential) = credential {
            let auth = json!({"kind":entry["kind"], "username":"refs-user", "secret":secret, "known_hosts":entry["known_hosts"].as_str().unwrap_or("")});
            let encrypted = crypto::seal(
                &database.state.settings.master_key,
                &serde_json::to_vec(&auth).unwrap(),
            )
            .unwrap();
            sqlx::query("INSERT INTO credentials(id,name,kind,ciphertext) VALUES($1,'refs',$2,$3)")
                .bind(credential)
                .bind(entry["kind"].as_str().unwrap())
                .bind(encrypted)
                .execute(&database.state.pool)
                .await
                .unwrap();
        }
        sqlx::query(
            "INSERT INTO projects(id,name,repository_url,credential_id) VALUES($1,'refs',$2,$3)",
        )
        .bind(project)
        .bind(entry["url"].as_str().unwrap())
        .bind(credential)
        .execute(&database.state.pool)
        .await
        .unwrap();
        let read_token = token(&database, "viewer", &[project], &["read"]).await;
        let response = request(&database, project, Some(&read_token)).await;
        assert_eq!(
            response.status().as_u16(),
            entry["status"].as_u64().unwrap() as u16,
            "{}",
            entry["name"]
        );
        let body = read_body(response).await;
        if entry["status"] == 200 {
            assert_eq!(body["default_ref"], "refs/heads/trunk");
            assert_eq!(body["branches"].as_array().unwrap().len(), 2);
            assert_eq!(body["tags"].as_array().unwrap().len(), 1);
            assert_eq!(body["branches"][0]["ref"], "refs/heads/trunk");
            assert_eq!(body["tags"][0]["ref"], "refs/tags/v1");
        } else {
            let message = body.to_string();
            assert!(!message.contains(&secret));
            assert!(!message.contains("BEGIN OPENSSH PRIVATE KEY"));
        }
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM builds")
            .fetch_one(&database.state.pool)
            .await
            .unwrap(),
        0
    );
    database.close().await;
}
