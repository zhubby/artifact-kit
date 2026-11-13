use artifactory_database::{PoolSettings, connect, with_database};
use artifactory_database::{params, sql};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL on an isolated TimescaleDB administrator database"]
async fn application_role_checks_extension_without_administrator_settings() {
    let url = std::env::var("ARTIFACTORY_TEST_DATABASE_URL").expect("test database URL");
    let mut admin = connect(&url, Default::default(), PoolSettings::new(2))
        .await
        .unwrap();
    let name = format!("startup_test_{}", Uuid::new_v4().simple());
    let password = Uuid::new_v4().simple().to_string();
    sql::execute(
        &mut admin,
        &format!("CREATE ROLE {name} LOGIN PASSWORD '{password}'"),
        params![],
    )
    .await
    .unwrap();
    sql::execute(
        &mut admin,
        &format!("CREATE DATABASE {name} TEMPLATE template0 OWNER {name}"),
        params![],
    )
    .await
    .unwrap();
    let database = with_database(&url, &name).unwrap();
    let mut identity = url::Url::parse(&database).unwrap();
    identity.set_username(&name).unwrap();
    identity.set_password(Some(&password)).unwrap();
    let pool = connect(identity.as_str(), Default::default(), PoolSettings::new(1))
        .await
        .unwrap();
    assert!(
        artifactory_database::require_timescale(&pool)
            .await
            .is_err()
    );
    let mut provisioner = connect(&database, Default::default(), PoolSettings::new(1))
        .await
        .unwrap();
    sql::execute(&mut provisioner, "CREATE EXTENSION timescaledb", params![])
        .await
        .unwrap();
    drop(provisioner);
    assert!(
        sql::execute(
            &mut pool.clone(),
            "SHOW shared_preload_libraries",
            params![]
        )
        .await
        .is_err(),
        "the application role must not need privileged configuration access"
    );
    artifactory_database::require_timescale(&pool)
        .await
        .unwrap();
    drop(pool);
    sql::execute(
        &mut admin,
        &format!("DROP DATABASE {name} WITH (FORCE)"),
        params![],
    )
    .await
    .unwrap();
    sql::execute(&mut admin, &format!("DROP ROLE {name}"), params![])
        .await
        .unwrap();
}
