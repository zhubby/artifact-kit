//! SQLx-compatible TLS URL options must never silently weaken transport security.

use toasty::db::Connect;

#[tokio::test]
async fn tls_mode_alias_rejects_invalid_or_incomplete_verification() {
    for parameter in ["sslmode", "ssl-mode"] {
        for mode in ["invalid", "verify-ca", "verify-full"] {
            let url = format!("postgres://localhost/example?{parameter}={mode}");
            assert!(
                Connect::new(&url).await.is_err(),
                "{parameter}={mode} must fail validation before opening a connection"
            );
            assert!(
                artifactory_database::listen(&url, "tls_test")
                    .await
                    .is_err(),
                "listener transport must enforce {parameter}={mode}"
            );
        }
    }
}

#[tokio::test]
async fn tls_root_certificate_aliases_validate_the_configured_file() {
    // A UUID directory makes the nonexistent path independent of host configuration.
    let missing = std::env::temp_dir()
        .join(format!("artifactory-tls-{}", uuid::Uuid::new_v4()))
        .join("ca.pem");
    for parameter in ["sslrootcert", "ssl-root-cert", "ssl-ca"] {
        let mut url = url::Url::parse("postgres://localhost/example?sslmode=require").unwrap();
        url.query_pairs_mut()
            .append_pair(parameter, missing.to_str().unwrap());
        assert!(
            Connect::new(url.as_str()).await.is_err(),
            "{parameter} must not ignore an unreadable CA file"
        );
        assert!(
            artifactory_database::listen(url.as_str(), "tls_test")
                .await
                .is_err(),
            "listener transport must validate {parameter}"
        );
    }
}

#[tokio::test]
async fn tls_client_identity_aliases_require_both_certificate_and_key() {
    for parameter in ["sslcert", "ssl-cert", "sslkey", "ssl-key"] {
        let url = format!("postgres://localhost/example?sslmode=require&{parameter}=unused.pem");
        assert!(
            Connect::new(&url).await.is_err(),
            "{parameter} must reject an incomplete client identity"
        );
    }
}

#[tokio::test]
async fn tls_mode_alias_preserves_disable_and_query_order() {
    for query in [
        "sslmode=require&ssl-mode=disable&sslcert=unused.pem",
        "ssl-mode=require&sslmode=disable&sslcert=unused.pem",
    ] {
        assert!(
            Connect::new(&format!("postgres://localhost/example?{query}"))
                .await
                .is_ok(),
            "the final disable setting must skip TLS configuration: {query}"
        );
    }
    for query in [
        "sslmode=disable&ssl-mode=verify-full",
        "ssl-mode=disable&sslmode=verify-full",
    ] {
        assert!(
            Connect::new(&format!("postgres://localhost/example?{query}"))
                .await
                .is_err(),
            "the final verification setting must require a CA: {query}"
        );
    }
}
