use crate::{protocol::Command, state::AppState};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
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
            &tokio::fs::read(
                std::env::var_os("ARTIFACTORY_EMISSARY_RELEASE_SOURCE")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| state.settings.data_dir.join("emissary-releases.json")),
            )
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
    if let Some(row) =
        sqlx::query("SELECT id,manifest_hash FROM emissary_releases WHERE version=$1")
            .bind(tag)
            .fetch_optional(&state.pool)
            .await?
    {
        ensure!(
            row.get::<String, _>("manifest_hash") == hash,
            "release version is immutable"
        );
        return Ok(row.get("id"));
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
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO emissary_releases(id,version,manifest,signature,manifest_hash) VALUES($1,$2,$3,$4,$5) ON CONFLICT(version) DO NOTHING").bind(id).bind(tag).bind(String::from_utf8(raw)?).bind(signature.trim()).bind(&hash).execute(&state.pool).await?;
    let row = sqlx::query("SELECT id,manifest_hash FROM emissary_releases WHERE version=$1")
        .bind(tag)
        .fetch_one(&state.pool)
        .await?;
    ensure!(
        row.get::<String, _>("manifest_hash") == hash,
        "release version is immutable"
    );
    Ok(row.get("id"))
}

// Pool advisory lock precedes the node row lock. Assignment uses the identical
// node row lock and excludes ota_draining, so neither can slip past the other.
pub async fn next(state: &AppState, node: Uuid, session: Uuid) -> Result<Option<Command>> {
    let pool: Uuid = sqlx::query_scalar("SELECT pool_id FROM nodes WHERE id=$1")
        .bind(node)
        .fetch_one(&state.pool)
        .await?;
    let mut tx = state.pool.begin().await?;
    let locked: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1,57))")
            .bind(pool.to_string())
            .fetch_one(&mut *tx)
            .await?;
    if !locked {
        tx.rollback().await?;
        return Ok(None);
    }
    let Some(n)=sqlx::query("SELECT n.*,coalesce(n.emissary_target_release,p.emissary_target_release) AS desired,p.emissary_update_concurrency,p.emissary_updates_paused FROM nodes n JOIN node_pools p ON p.id=n.pool_id WHERE n.id=$1 AND n.pool_id=$2 AND n.active_session=$3 AND NOT n.revoked AND n.certificate_expires_at>clock_timestamp() AND n.last_seen>now()-interval '30 seconds' FOR UPDATE OF n SKIP LOCKED").bind(node).bind(pool).bind(session).fetch_optional(&mut *tx).await? else{tx.rollback().await?;return Ok(None)};
    let mut update =
        sqlx::query("SELECT * FROM emissary_updates WHERE node_id=$1 AND finished_at IS NULL")
            .bind(node)
            .fetch_optional(&mut *tx)
            .await?;
    if update.is_none() {
        let Some(target) = n.get::<Option<Uuid>, _>("desired") else {
            tx.rollback().await?;
            return Ok(None);
        };
        if n.get::<bool, _>("emissary_updates_paused")
            || !n
                .get::<Value, _>("capabilities")
                .as_array()
                .is_some_and(|a| a.contains(&json!("emissary-ota-v1")))
        {
            tx.rollback().await?;
            return Ok(None);
        }
        let version: String =
            sqlx::query_scalar("SELECT version FROM emissary_releases WHERE id=$1")
                .bind(target)
                .fetch_one(&mut *tx)
                .await?;
        if n.get::<Option<String>, _>("emissary_current_version")
            .as_deref()
            == Some(&version)
        {
            tx.rollback().await?;
            return Ok(None);
        }
        let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM emissary_updates WHERE node_id=$1 AND release_id=$2 AND status IN ('failed','rolled_back','canceled'))").bind(node).bind(target).fetch_one(&mut *tx).await?;
        if blocked {
            tx.rollback().await?;
            return Ok(None);
        }
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM emissary_updates WHERE pool_id=$1 AND finished_at IS NULL",
        )
        .bind(pool)
        .fetch_one(&mut *tx)
        .await?;
        if count >= i64::from(n.get::<i32, _>("emissary_update_concurrency")) {
            tx.rollback().await?;
            return Ok(None);
        }
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO emissary_updates(id,node_id,pool_id,release_id,previous_version) VALUES($1,$2,$3,$4,$5)").bind(id).bind(node).bind(pool).bind(target).bind(n.get::<Option<String>,_>("emissary_current_version")).execute(&mut *tx).await?;
        sqlx::query("UPDATE nodes SET ota_draining=true WHERE id=$1")
            .bind(node)
            .execute(&mut *tx)
            .await?;
        update = sqlx::query("SELECT * FROM emissary_updates WHERE id=$1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    }
    let u = update.unwrap();
    if u.get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_command_at")
        .is_some_and(|t| chrono::Utc::now() - t < chrono::Duration::seconds(10))
    {
        tx.rollback().await?;
        return Ok(None);
    }
    let id: Uuid = u.get("id");
    let r = sqlx::query("SELECT * FROM emissary_releases WHERE id=$1")
        .bind(u.get::<Uuid, _>("release_id"))
        .fetch_one(&mut *tx)
        .await?;
    let raw: String = r.get("manifest");
    let m: Value = serde_json::from_str(&raw)?;
    let a = m["assets"]
        .as_array()
        .context("invalid stored manifest")?
        .iter()
        .find(|a| {
            a["os"].as_str() == Some(n.get::<String, _>("os").as_str())
                && a["arch"].as_str() == Some(n.get::<String, _>("arch").as_str())
        })
        .context("platform not present")?;
    let url = state.storage.emissary_download_url(&format!(
        "emissary-releases/{}/{}",
        r.get::<String, _>("manifest_hash"),
        a["name"].as_str().context("invalid asset")?
    ));
    sqlx::query("UPDATE emissary_updates SET last_command_at=now(),status=CASE WHEN status='queued' THEN 'offered' ELSE status END WHERE id=$1").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(Command{command_id:id.to_string(),kind:"emissary_update".into(),payload_json:json!({"id":id,"generation":u.get::<i64,_>("generation"),"action":if u.get::<bool,_>("cancel_requested"){"cancel"}else{"apply"},"manifest":raw,"signature":r.get::<String,_>("signature"),"url":url}).to_string(),..Default::default()}))
}

// Called only from the authenticated current mTLS stream. The node lock is
// revalidated inside this transaction, including revocation and cert expiry.
pub async fn report(state: &AppState, node: Uuid, session: Uuid, payload: &Value) -> Result<bool> {
    let Some(status) = payload.get("ota").filter(|s| s.is_object()) else {
        return Ok(false);
    };
    let update_id = status["update_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok());
    let runtime = payload["diagnostics"]["emissary_version"]
        .as_str()
        .unwrap_or("");
    if update_id.is_none() {
        // No rollout state is changed. One conditional UPDATE both authorizes
        // the current session and records health, without a separate transaction
        // and repeated node reads on every idle device heartbeat.
        let updated=sqlx::query("UPDATE nodes SET emissary_current_version=$3,emissary_launcher_version=$4,emissary_ota_status=$5 WHERE id=$1 AND active_session=$2 AND NOT revoked AND certificate_expires_at>clock_timestamp()")
            .bind(node).bind(session).bind(runtime).bind(status["launcher_version"].as_str()).bind(status)
            .execute(&state.pool).await?;
        ensure!(
            updated.rows_affected() == 1,
            "node session revoked or superseded"
        );
        return Ok(true);
    }
    let mut tx = state.pool.begin().await?;
    // Ordinary health reports do not change rollout admission. Serialize with
    // pool admission only when reporting a specific update, rather than making
    // every idle device heartbeat wait on the same pool-wide lock.
    if update_id.is_some() {
        let pool: Uuid = sqlx::query_scalar("SELECT pool_id FROM nodes WHERE id=$1")
            .bind(node)
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,57))")
            .bind(pool.to_string())
            .execute(&mut *tx)
            .await?;
    }
    let valid:Option<Uuid>=sqlx::query_scalar("SELECT id FROM nodes WHERE id=$1 AND active_session=$2 AND NOT revoked AND certificate_expires_at>clock_timestamp() FOR UPDATE").bind(node).bind(session).fetch_optional(&mut *tx).await?;
    ensure!(valid.is_some(), "node session revoked or superseded");
    let still_valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND active_session=$2 AND NOT revoked AND certificate_expires_at>clock_timestamp())").bind(node).bind(session).fetch_one(&mut *tx).await?;
    ensure!(
        still_valid,
        "node certificate expired while waiting for its lock"
    );
    sqlx::query("UPDATE nodes SET emissary_current_version=$2,emissary_launcher_version=$3,emissary_ota_status=$4 WHERE id=$1").bind(node).bind(runtime).bind(status["launcher_version"].as_str()).bind(status).execute(&mut *tx).await?;
    let mut health = true;
    if let Some(id) = update_id
        && let Some(u)=sqlx::query("SELECT u.*,r.version FROM emissary_updates u JOIN emissary_releases r ON r.id=u.release_id WHERE u.id=$1 AND u.node_id=$2 AND u.finished_at IS NULL").bind(id).bind(node).fetch_optional(&mut *tx).await? {
            let phase=status["phase"].as_str().unwrap_or("");
            let terminal=["succeeded","failed","rolled_back","canceled"].contains(&phase);
            if phase=="activating"||phase=="succeeded" {
                health=runtime==u.get::<String,_>("version")
                    || (phase=="activating"&&status["activation_pending"]==true&&Some(runtime)==u.get::<Option<String>,_>("previous_version").as_deref());
            }
            if ["downloading","draining","activating","succeeded","failed","rolled_back","canceled"].contains(&phase)&&health {
                sqlx::query("UPDATE emissary_updates SET status=$2,error=$3,updated_at=now(),finished_at=CASE WHEN $4 THEN now() ELSE NULL END WHERE id=$1").bind(id).bind(phase).bind(status["error"].as_str().map(|s|s.chars().take(1000).collect::<String>())).bind(terminal).execute(&mut *tx).await?;
                if terminal {
                    sqlx::query("UPDATE nodes SET ota_draining=false WHERE id=$1").bind(node).execute(&mut *tx).await?;
                    if phase=="failed"||phase=="rolled_back" {sqlx::query("UPDATE node_pools SET emissary_updates_paused=true WHERE id=$1").bind(u.get::<Uuid,_>("pool_id")).execute(&mut *tx).await?;}
                    sqlx::query("INSERT INTO audit_events(actor,action,resource_id,details) VALUES('emissary','emissary_update_finished',$1,$2)").bind(id.to_string()).bind(json!({"node_id":node,"status":phase,"error":status["error"]})).execute(&mut *tx).await?;
                }
            }
    }
    tx.commit().await?;
    Ok(health)
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
