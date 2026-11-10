use crate::settings::Settings;
use anyhow::{Context, Result, ensure};
use tokio::process::Command;
use uuid::Uuid;

pub async fn sign(settings: &Settings, node_id: Uuid, csr: &str) -> Result<String> {
    ensure!(
        csr.len() <= 16384 && csr.starts_with("-----BEGIN CERTIFICATE REQUEST-----"),
        "invalid CSR"
    );
    let directory = tempfile::tempdir_in(&settings.data_dir)?;
    let request = directory.path().join("request.pem");
    let certificate = directory.path().join("certificate.pem");
    let extensions = directory.path().join("extensions.conf");
    tokio::fs::write(&request, csr).await?;
    tokio::fs::write(&extensions, "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=clientAuth\n").await?;
    let output = Command::new("openssl")
        .args(["x509", "-req", "-sha256", "-days", "30", "-in"])
        .arg(&request)
        .arg("-CA")
        .arg(settings.pki_dir.join("ca.pem"))
        .arg("-CAkey")
        .arg(settings.pki_dir.join("ca.key"))
        .arg("-set_serial")
        .arg(format!("0x{}", node_id.simple()))
        .arg("-subj")
        .arg(format!("/CN={node_id}"))
        .arg("-extfile")
        .arg(extensions)
        .arg("-out")
        .arg(&certificate)
        .kill_on_drop(true)
        .output()
        .await
        .context("openssl is required for node enrollment")?;
    ensure!(output.status.success(), "certificate request rejected");
    Ok(tokio::fs::read_to_string(certificate).await?)
}

pub fn certificate_hash(pem: &str) -> Result<String> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let encoded: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    Ok(crate::crypto::hash(STANDARD.decode(encoded)?))
}
