use sqlx::{
    Connection, PgConnection,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL on an isolated TimescaleDB administrator database"]
async fn application_role_checks_extension_without_administrator_settings() {
    let options: PgConnectOptions = std::env::var("ARTIFACTORY_TEST_DATABASE_URL")
        .expect("test database URL")
        .parse()
        .unwrap();
    let mut admin = PgConnection::connect_with(&options).await.unwrap();
    let name = format!("startup_test_{}", Uuid::new_v4().simple());
    let password = Uuid::new_v4().simple().to_string();
    sqlx::query(&format!("CREATE ROLE {name} LOGIN PASSWORD '{password}'"))
        .execute(&mut admin)
        .await
        .unwrap();
    sqlx::query(&format!(
        "CREATE DATABASE {name} TEMPLATE template0 OWNER {name}"
    ))
    .execute(&mut admin)
    .await
    .unwrap();
    let database = options.database(&name);
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(database.clone().username(&name).password(&password))
        .await
        .unwrap();
    assert!(
        artifactory_database::require_timescale(&pool)
            .await
            .is_err()
    );
    let mut provisioner = PgConnection::connect_with(&database).await.unwrap();
    sqlx::query("CREATE EXTENSION timescaledb")
        .execute(&mut provisioner)
        .await
        .unwrap();
    provisioner.close().await.unwrap();
    assert!(
        sqlx::query("SHOW shared_preload_libraries")
            .execute(&pool)
            .await
            .is_err(),
        "the application role must not need privileged configuration access"
    );
    artifactory_database::require_timescale(&pool)
        .await
        .unwrap();
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {name}"))
        .execute(&mut admin)
        .await
        .unwrap();
    sqlx::query(&format!("DROP ROLE {name}"))
        .execute(&mut admin)
        .await
        .unwrap();
}
