use crate::{protocol::Command, state::AppState};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub os: String,
    pub arch: String,
    pub name: String,
    pub size: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: String,
    pub commit: String,
    pub protocol_min: u32,
    pub protocol_max: u32,
    pub assets: Vec<Asset>,
}
pub fn safe_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 100
        && tag.as_bytes()[0].is_ascii_alphanumeric()
        && tag
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}
pub fn verify(raw: &[u8], signature: &str, keys: &[String]) -> Result<Manifest> {
    ensure!(raw.len() <= 65536, "oversized manifest");
    let sig = Signature::from_slice(&STANDARD.decode(signature.trim())?)?;
    ensure!(
        keys.iter().any(|key| {
            STANDARD
                .decode(key)
                .ok()
                .and_then(|b| <[u8; 32]>::try_from(b).ok())
                .and_then(|b| VerifyingKey::from_bytes(&b).ok())
                .is_some_and(|key| key.verify_strict(raw, &sig).is_ok())
        }),
        "release signature rejected"
    );
    let m: Manifest = serde_json::from_slice(raw)?;
    ensure!(
        safe_tag(&m.version)
            && m.commit.len() == 40
            && m.commit.bytes().all(|c| c.is_ascii_hexdigit())
            && m.protocol_min == 1
            && m.protocol_max >= 1,
        "incompatible manifest version or protocol"
    );
    ensure!(
        m.assets.len() == 6,
        "release must contain all six native platforms"
    );
    let mut seen = std::collections::HashSet::new();
    for a in &m.assets {
        let platform = format!("{}/{}", a.os, a.arch);
        ensure!(
            [
                "linux/amd64",
                "linux/arm64",
                "darwin/amd64",
                "darwin/arm64",
                "windows/amd64",
                "windows/arm64"
            ]
            .contains(&platform.as_str())
                && seen.insert(platform),
            "invalid or duplicate platform"
        );
        ensure!(
            a.name
                == format!(
                    "artifactory-emissary-{}-{}{}",
                    a.os,
                    a.arch,
                    if a.os == "windows" { ".exe" } else { "" }
                )
                && a.size > 0
                && a.size <= 128 * 1024 * 1024
                && a.sha256.len() == 64
                && a.sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid release asset"
        );
    }
    Ok(m)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseSource {
    pub repository: String,
    #[serde(default = "github_api")]
    pub api_base: String,
    pub public_keys: Vec<String>,
}
fn github_api() -> String {
    "https://api.github.com".into()
}
impl ReleaseSource {
    pub async fn load(state: &AppState) -> Result<Self> {
        let c: Self = serde_json::from_slice(
            &tokio::fs::read(state.settings.data_dir.join("emissary-releases.json"))
                .await
                .context("Emissary release source is not configured")?,
        )?;
        let parts: Vec<_> = c.repository.split('/').collect();
        ensure!(
            parts.len() == 2 && parts.iter().all(|p| safe_tag(p)),
            "invalid GitHub repository"
        );
        let u: url::Url = c.api_base.parse()?;
        ensure!(
            u.host_str().is_some()
                && u.username().is_empty()
                && u.password().is_none()
                && u.query().is_none()
                && u.fragment().is_none()
                && (u.scheme() == "https" || (state.settings.insecure_dev && u.scheme() == "http")),
            "invalid configured GitHub API"
        );
        ensure!(
            !c.public_keys.is_empty(),
            "release verification keys required"
        );
        Ok(c)
    }
}
async fn bounded(response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    use futures::StreamExt;
    let mut stream = response.error_for_status()?.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        ensure!(
            body.len() + chunk.len() <= limit,
            "release asset exceeds declared limit"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub async fn import_tag(state: &AppState, tag: &str) -> Result<Uuid> {
    ensure!(safe_tag(tag), "invalid release tag");
    let source = ReleaseSource::load(state).await?;
    let origin = url::Url::parse(&source.api_base)?;
    let origin_host = origin.host_str().unwrap().to_string();
    let allow_http = state.settings.insecure_dev;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            let url = attempt.url();
            let host = url.host_str().unwrap_or("");
            if attempt.previous().len() < 5
                && (url.scheme() == "https" || (allow_http && url.scheme() == "http"))
                && (host == origin_host
                    || host == "github.com"
                    || host.ends_with(".githubusercontent.com"))
            {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()?;
    let api = format!(
        "{}/repos/{}",
        source.api_base.trim_end_matches('/'),
        source.repository
    );
    let get = |url: String, accept: &str| {
        let mut r = client
            .get(url)
            .header("User-Agent", "artifactory-emissary-ota")
            .header("Accept", accept);
        if let Ok(token) = std::env::var("ARTIFACTORY_EMISSARY_GITHUB_TOKEN") {
            r = r.bearer_auth(token)
        }
        r
    };
    let release: Value = serde_json::from_slice(
        &bounded(
            get(
                format!("{api}/releases/tags/{tag}"),
                "application/vnd.github+json",
            )
            .send()
            .await?,
            1024 * 1024,
        )
        .await?,
    )?;
    ensure!(
        release["tag_name"].as_str() == Some(tag) && release["draft"] != true,
        "release tag mismatch or draft"
    );
    let assets = release["assets"]
        .as_array()
        .context("release assets missing")?;
    let asset_url = |name: &str| -> Result<String> {
        let matching: Vec<_> = assets
            .iter()
            .filter(|a| a["name"].as_str() == Some(name))
            .collect();
        ensure!(
            matching.len() == 1,
            "release asset missing or duplicate: {name}"
        );
        let id = matching[0]["id"]
            .as_u64()
            .context("invalid GitHub asset ID")?;
        Ok(format!("{api}/releases/assets/{id}"))
    };
    let raw = bounded(
        get(
            asset_url("emissary-manifest.json")?,
            "application/octet-stream",
        )
        .send()
        .await?,
        65536,
    )
    .await?;
    let signature = String::from_utf8(
        bounded(
            get(
                asset_url("emissary-manifest.sig")?,
                "application/octet-stream",
            )
            .send()
            .await?,
            1024,
        )
        .await?,
    )?;
    let manifest = verify(&raw, &signature, &source.public_keys)?;
    ensure!(
        manifest.version == tag,
        "manifest does not match release tag"
    );
    let hash = hex::encode(Sha256::digest(&raw));
    if let Some(id) = state.store.existing_emissary_release(tag, &hash).await? {
        return Ok(id);
    }
    let temp = tempfile::tempdir()?;
    for asset in &manifest.assets {
        let body = bounded(
            get(asset_url(&asset.name)?, "application/octet-stream")
                .send()
                .await?,
            asset.size as usize,
        )
        .await?;
        ensure!(
            body.len() as u64 == asset.size && hex::encode(Sha256::digest(&body)) == asset.sha256,
            "release binary verification failed: {}",
            asset.name
        );
        tokio::fs::write(temp.path().join(&asset.name), body).await?;
    }
    for asset in &manifest.assets {
        state
            .storage
            .put(
                &format!("emissary-releases/{hash}/{}", asset.name),
                tokio::fs::read(temp.path().join(&asset.name)).await?,
            )
            .await?;
    }
    state
        .store
        .import_emissary_release(tag, String::from_utf8(raw)?, signature.trim(), &hash)
        .await
}

// Pool advisory lock precedes the node row lock. Assignment uses the identical
// node row lock and excludes ota_draining, so neither can slip past the other.
pub async fn next(state: &AppState, node: Uuid, session: Uuid) -> Result<Option<Command>> {
    let Some(offer) = state.store.next_emissary_update(node, session).await? else {
        return Ok(None);
    };
    let url = state.storage.emissary_download_url(&offer.object_key);
    Ok(Some(Command { command_id: offer.id.to_string(), kind: "emissary_update".into(),
        payload_json: json!({"id":offer.id,"generation":offer.generation,"action":if offer.cancel {"cancel"} else {"apply"},"manifest":offer.manifest,"signature":offer.signature,"url":url}).to_string(), ..Default::default() }))
}

// Called only from the authenticated current mTLS stream. The node lock is
// revalidated inside this transaction, including revocation and cert expiry.
pub async fn report(state: &AppState, node: Uuid, session: Uuid, payload: &Value) -> Result<bool> {
    state
        .store
        .report_emissary_update(node, session, payload)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use ed25519_dalek::{Signer, SigningKey};
    #[test]
    fn signed_release_requires_all_platforms_and_protocol_one() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let platforms = [
            ("linux", "amd64"),
            ("linux", "arm64"),
            ("darwin", "amd64"),
            ("darwin", "arm64"),
            ("windows", "amd64"),
            ("windows", "arm64"),
        ];
        let mut m = Manifest {
            version: "v1.2.3".into(),
            commit: "a".repeat(40),
            protocol_min: 1,
            protocol_max: 1,
            assets: platforms
                .iter()
                .map(|(os, arch)| Asset {
                    os: (*os).into(),
                    arch: (*arch).into(),
                    name: format!(
                        "artifactory-emissary-{os}-{arch}{}",
                        if *os == "windows" { ".exe" } else { "" }
                    ),
                    size: 10,
                    sha256: "b".repeat(64),
                })
                .collect(),
        };
        let keys = vec![STANDARD.encode(key.verifying_key().as_bytes())];
        let raw = serde_json::to_vec(&m).unwrap();
        let sig = STANDARD.encode(key.sign(&raw).to_bytes());
        assert_eq!(verify(&raw, &sig, &keys).unwrap().version, "v1.2.3");
        assert!(verify(&raw, &STANDARD.encode([0; 64]), &keys).is_err());
        assert!(verify(&[raw.clone(), b" ".to_vec()].concat(), &sig, &keys).is_err());
        m.protocol_min = 2;
        m.protocol_max = 2;
        let raw = serde_json::to_vec(&m).unwrap();
        assert!(verify(&raw, &STANDARD.encode(key.sign(&raw).to_bytes()), &keys).is_err());
        m.protocol_min = 1;
        m.protocol_max = 1;
        m.assets.pop();
        let raw = serde_json::to_vec(&m).unwrap();
        assert!(verify(&raw, &STANDARD.encode(key.sign(&raw).to_bytes()), &keys).is_err());
    }
}
