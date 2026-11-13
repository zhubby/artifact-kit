use super::business_authorization;
use anyhow::{Result, ensure};
use artifactory_database::{PoolSettings, connect, migrate, params, sql, with_database};
use toasty::migration::{MigrationFile, MigrationSet};
use uuid::Uuid;

static BUSINESS: MigrationSet = MigrationSet::new(&[MigrationFile::new(
    2026111301,
    "0000_baseline.sql",
    "CREATE TABLE nodes(id uuid PRIMARY KEY,certificate_hash text,revoked boolean NOT NULL DEFAULT false,certificate_expires_at timestamptz NOT NULL DEFAULT now()+interval '1 day');
     CREATE TABLE attempts(id uuid PRIMARY KEY,node_id uuid NOT NULL);
     CREATE TABLE private_business_data(secret text);",
)]);

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL on an isolated TimescaleDB administrator database"]
async fn business_authorization_starts_with_only_documented_read_grants() -> Result<()> {
    let url = std::env::var("ARTIFACTORY_TEST_DATABASE_URL")?;
    let mut admin = connect(&url, Default::default(), PoolSettings::new(1)).await?;
    let name = format!("telemetry_auth_{}", Uuid::new_v4().simple());
    let password = Uuid::new_v4().simple().to_string();
    sql::execute(
        &mut admin,
        &format!("CREATE ROLE {name} LOGIN NOINHERIT PASSWORD '{password}'"),
        params![],
    )
    .await?;
    let result = async {
        sql::execute(
            &mut admin,
            &format!("CREATE DATABASE {name} TEMPLATE template0"),
            params![],
        )
        .await?;
        let scoped = with_database(&url, &name)?;
        let mut owner = connect(&scoped, Default::default(), PoolSettings::new(1)).await?;
        sql::execute(&mut owner, "CREATE EXTENSION timescaledb", params![]).await?;
        migrate(&owner, &BUSINESS).await?;
        let node = Uuid::new_v4();
        let attempt = Uuid::new_v4();
        sql::execute(
            &mut owner,
            "INSERT INTO nodes(id,certificate_hash) VALUES($1,'test-certificate')",
            params![node],
        )
        .await?;
        sql::execute(
            &mut owner,
            "INSERT INTO attempts(id,node_id) VALUES($1,$2)",
            params![attempt, node],
        )
        .await?;
        // Reproduce the previously documented role without any inherited public grants.
        for statement in [
            format!("REVOKE ALL ON DATABASE {name} FROM PUBLIC"),
            "REVOKE ALL ON SCHEMA public FROM PUBLIC".into(),
            format!("GRANT CONNECT ON DATABASE {name} TO {name}"),
            format!("GRANT USAGE ON SCHEMA public TO {name}"),
            format!("GRANT SELECT ON nodes,attempts TO {name}"),
        ] {
            sql::execute(&mut owner, &statement, params![]).await?;
        }
        let mut identity = url::Url::parse(&scoped)?;
        identity.set_username(&name).expect("valid test role name");
        identity
            .set_password(Some(&password))
            .expect("valid test role password");
        ensure!(
            business_authorization(identity.as_str()).await.is_err(),
            "a role without migration metadata access must fail startup validation"
        );

        // Execute the provisioning instructions themselves, using only UUID identifiers.
        let grants = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../deploy/telemetry-business-readonly.sql"
        ))
        .replace(":\"business_database\"", &format!("\"{name}\""))
        .replace(":\"business_schema\"", "\"public\"")
        .replace(":\"telemetry_business_role\"", &format!("\"{name}\""));
        for statement in grants.split(';').filter(|s| !s.trim().is_empty()) {
            sql::execute(&mut owner, statement, params![]).await?;
        }
        let authorization = business_authorization(identity.as_str()).await?;
        ensure!(
            authorization.identify("test-certificate".into()).await? == node,
            "the restricted role must identify registered nodes"
        );
        authorization.authorize_attempts(node, &[attempt]).await?;
        ensure!(
            authorization
                .authorize_attempts(node, &[Uuid::new_v4()])
                .await
                .is_err(),
            "unknown attempts must still be rejected"
        );
        ensure!(
            sql::scalar::<String>(
                &mut authorization.pool.clone(),
                "SHOW transaction_read_only",
                params![],
            )
            .await?
                == "on",
            "the authorization connection must start in read-only mode"
        );
        ensure!(
            sql::execute(
                &mut authorization.pool.clone(),
                "UPDATE nodes SET revoked=true",
                params![],
            )
            .await
            .is_err(),
            "the startup connection must reject business writes"
        );
        // Prove the role's ACLs, independently of the connection's read-only setting.
        let mut restricted =
            connect(identity.as_str(), Default::default(), PoolSettings::new(1)).await?;
        ensure!(
            sql::scalar::<String>(&mut restricted, "SHOW transaction_read_only", params![]).await?
                == "off",
            "the ACL check must not rely on a read-only connection setting"
        );
        for statement in [
            "UPDATE nodes SET revoked=true",
            "DELETE FROM attempts",
            "UPDATE __toasty_migrations SET id=0",
            "SELECT * FROM private_business_data",
        ] {
            ensure!(
                sql::execute(&mut restricted, statement, params![])
                    .await
                    .is_err(),
                "the documented grants must not permit {statement}"
            );
        }
        // Control's incremental HA migration preserves these read-only columns.
        sql::execute(&mut owner, "INSERT INTO __toasty_migrations(id,name,applied_at) VALUES(2026111401,'0001_control_replication.sql',now())", params![]).await?;
        let upgraded = business_authorization(identity.as_str()).await?;
        ensure!(upgraded.identify("test-certificate".into()).await? == node,
            "the restricted role must start against the known HA history");
        upgraded.authorize_attempts(node, &[attempt]).await?;
        ensure!(sql::execute(&mut upgraded.pool.clone(), "UPDATE nodes SET revoked=true", params![]).await.is_err(),
            "HA startup must preserve read-only authorization");
        sql::execute(&mut owner, "UPDATE __toasty_migrations SET id=0 WHERE id=2026111301", params![]).await?;
        ensure!(
            business_authorization(identity.as_str()).await.is_err(),
            "a readable but unrecognized migration must still prevent startup"
        );
        Ok(())
    }
    .await;
    sql::execute(
        &mut admin,
        &format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
        params![],
    )
    .await?;
    sql::execute(&mut admin, &format!("DROP ROLE {name}"), params![]).await?;
    result
}
