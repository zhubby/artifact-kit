use artifactory_emissary::{artifacts, config::Config, ota, worker::Lease};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use std::{
    fs,
    time::{Duration, Instant},
};

#[test]
fn collect_rejects_escapes_and_symlinks() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("out")).unwrap();
    fs::write(root.path().join("out/a.txt"), "artifact").unwrap();
    for pattern in [
        "../*", "/etc/*", "out/../*", "out\\*", "C:/*", "", "./out/*",
    ] {
        assert!(
            artifacts::collect(root.path(), &[pattern.into()]).is_err(),
            "{pattern}"
        );
    }
    assert_eq!(
        artifacts::collect(root.path(), &["out/**/*.txt".into()]).unwrap(),
        ["out/a.txt"]
    );
    assert!(artifacts::collect(root.path(), &["missing/*".into()]).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.path().join("out"), root.path().join("link")).unwrap();
        assert!(artifacts::collect(root.path(), &["link/*".into()]).is_err());
        assert!(artifacts::open(root.path(), "link/a.txt").is_err());
        std::os::unix::fs::symlink("a.txt", root.path().join("out/b.txt")).unwrap();
        assert!(artifacts::collect(root.path(), &["out/*".into()]).is_err());
        assert!(artifacts::open(root.path(), "out/b.txt").is_err());
    }
}

#[test]
fn lease_uses_local_monotonic_deadline() {
    let now = Instant::now();
    let mut lease = Lease::new(now, 30);
    assert!(!lease.expired(now + Duration::from_secs(29)));
    assert!(lease.expired(now + Duration::from_secs(30)));
    assert!(!lease.renew(now + Duration::from_secs(31), now, 90));
    let mut lease = Lease::new(now, 90);
    assert!(lease.renew(now + Duration::from_secs(15), now, 30));
    assert_eq!(lease.deadline(), now + Duration::from_secs(30));
}

#[test]
fn signed_package_verification() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let body = b"new emissary executable";
    let asset = ota::Asset::from_bytes("linux", "arm64", body);
    let manifest = ota::Manifest {
        version: "emissary-v1.0.0".into(),
        commit: "a".repeat(40),
        protocol_min: 1,
        protocol_max: 1,
        assets: vec![asset.clone()],
    };
    let raw = serde_json::to_vec(&manifest).unwrap();
    let sig = STANDARD.encode(key.sign(&raw).to_bytes());
    let keys = [STANDARD.encode(key.verifying_key().to_bytes())];
    assert_eq!(
        ota::verify(&raw, &sig, &keys, "linux", "arm64").unwrap(),
        asset
    );
    assert!(
        ota::verify(
            &[raw.as_slice(), b" "].concat(),
            &sig,
            &keys,
            "linux",
            "arm64"
        )
        .is_err()
    );
    assert!(ota::verify(&raw, &sig, &keys, "darwin", "arm64").is_err());
    assert!(ota::verify_bytes(b"wrong", &asset).is_err());
    ota::verify_bytes(body, &asset).unwrap();
}

#[test]
fn interrupted_activation_rolls_back_once() {
    let mut state = ota::State {
        phase: "activating".into(),
        current: ota::Image {
            version: "old".into(),
            path: "old".into(),
        },
        candidate: ota::Image {
            version: "new".into(),
            path: "new".into(),
        },
        ..Default::default()
    };
    assert!(state.recover());
    assert_eq!(state.phase, "rolled_back");
    assert_eq!(state.current.version, "old");
    assert!(state.candidate.path.is_empty());
    assert!(!state.recover());
    state.phase = "downloading".into();
    assert!(state.recover());
    assert_eq!(state.phase, "failed");
    assert_eq!(state.current.version, "old");
}

#[test]
fn health_requires_consecutive_session_confirmations() {
    let mut health = ota::HealthWindow::default();
    let now = Instant::now();
    assert!(!health.ack("a", "1", now));
    assert!(!health.ack("a", "1", now));
    assert!(!health.ack("a", "2", now));
    assert!(!health.ack("b", "3", now));
    assert!(!health.ack("b", "4", now));
    assert!(health.ack("b", "5", now));
    assert!(!health.ack("b", "6", now + Duration::from_secs(26)));
    assert!(!health.ack("b", "", now + Duration::from_secs(26)));
}

#[test]
fn strict_private_config() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("emissary.json");
    fs::write(
        &path,
        r#"{"endpoint":"localhost:9443","ca_file":"ca.pem","root":"runtime","agent_id":"old"}"#,
    )
    .unwrap();
    assert!(Config::load(&path).is_err());
    let cfg = Config {
        endpoint: "localhost:9443".into(),
        ca_file: "ca.pem".into(),
        root: directory.path().join("runtime"),
        ..Default::default()
    };
    cfg.save(&path).unwrap();
    let loaded = Config::load(&path).unwrap();
    assert_eq!(loaded.log_buffer_bytes, 256 << 20);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
