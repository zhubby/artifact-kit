use artifactory_control_plane::{settings::Settings, storage::Storage};
use std::process::Command;

#[test]
fn storage_uses_system_trust_and_verifies_s3_identity() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/storage_tls.py"
        ))
        .arg(std::env::current_exe().unwrap())
        .arg(directory.path())
        .output()
        .expect("run real HTTPS storage fixture");
    assert!(
        output.status.success(),
        "storage TLS acceptance failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// A separate process is required: SSL_CERT_FILE / SSL_CERT_DIR belong to the
// client's process environment and must not race other Rust tests.
#[tokio::test]
async fn storage_tls_probe_helper() {
    if std::env::var("ARTIFACTORY_TEST_STORAGE_TLS_HELPER").as_deref() != Ok("1") {
        return;
    }
    let endpoint = std::env::var("ARTIFACTORY_TEST_STORAGE_TLS_ENDPOINT").unwrap();
    let directory = std::env::var("ARTIFACTORY_TEST_STORAGE_TLS_DIRECTORY").unwrap();
    let settings = Settings {
        database_url: "unused".into(),
        http_bind: "unused".into(),
        grpc_bind: "unused".into(),
        public_url: endpoint.clone(),
        insecure_dev: false,
        registration_enabled: true,
        pki_dir: directory.clone().into(),
        data_dir: directory.into(),
        master_key: [0; 32],
        s3_endpoint: endpoint.clone(),
        s3_public_endpoint: endpoint,
        s3_bucket: "tls-probe".into(),
        s3_region: "us-east-1".into(),
        s3_access_key: "non-secret-fixture".into(),
        s3_secret_key: "non-secret-fixture".into(),
        telemetry: None,
    };
    if let Err(error) = Storage::new(&settings).unwrap().initialize().await {
        panic!("actual Storage initialization failed: {error:#}");
    }
}
