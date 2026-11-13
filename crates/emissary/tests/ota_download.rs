use artifactory_emissary::{ota, platform};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use std::fs;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn interrupted_download_never_replaces_current_binary() {
    let key = SigningKey::from_bytes(&[9; 32]);
    let body = b"complete release binary";
    let (os, arch) = platform();
    let asset = ota::Asset::from_bytes(os, arch, body);
    let manifest = ota::Manifest {
        version: "v-test".into(),
        commit: "a".repeat(40),
        protocol_min: 1,
        protocol_max: 1,
        assets: vec![asset.clone()],
    };
    let raw = serde_json::to_string(&manifest).unwrap();
    for bad in [
        body[..3].to_vec(),
        vec![b'x'; body.len()],
        [body.as_slice(), &[1]].concat(),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let current = directory.path().join("current");
        fs::write(&current, b"known good").unwrap();
        let app = axum::Router::new().route(
            "/binary",
            axum::routing::get(move || {
                let bad = bad.clone();
                async move { bad }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cfg = ota::OtaConfig {
            directory: directory.path().into(),
            public_keys: vec![STANDARD.encode(key.verifying_key().to_bytes())],
            allow_http: true,
        };
        let request = ota::Request {
            manifest: raw.clone(),
            signature: STANDARD.encode(key.sign(raw.as_bytes()).to_bytes()),
            url: format!("http://{address}/binary"),
            ..Default::default()
        };
        assert!(
            ota::download(&cfg, &request, &CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(fs::read(&current).unwrap(), b"known good");
        assert!(
            !directory
                .path()
                .join("versions")
                .join(&asset.sha256)
                .join(ota::binary_name())
                .exists()
        );
        server.abort();
    }
}
