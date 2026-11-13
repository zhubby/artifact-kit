//! OTA release import and serialized update coordination.
use super::ControlStore;
use anyhow::{Context, Result, ensure};
use artifactory_database::{params, sql};
use serde_json::{Value, json};
use toasty::stmt::Type;

use uuid::Uuid;
pub(crate) struct OtaOffer {
    pub id: Uuid,
    pub generation: i64,
    pub cancel: bool,
    pub manifest: String,
    pub signature: String,
    pub object_key: String,
}
impl ControlStore {
    pub(crate) async fn existing_emissary_release(
        &self,
        tag: &str,
        hash: &str,
    ) -> Result<Option<Uuid>> {
        if let Some(row) = sql::optional_record(
            &mut self.pool.clone(),
            "SELECT id,manifest_hash FROM emissary_releases WHERE version=$1",
            params![tag],
            &[("id", Type::Uuid), ("manifest_hash", Type::String)],
        )
        .await?
        {
            ensure!(
                row.decode::<String>("manifest_hash")? == hash,
                "release version is immutable"
            );
            return Ok(Some(row.decode("id")?));
        }
        Ok(None)
    }

    pub(crate) async fn import_emissary_release(
        &self,
        tag: &str,
        raw: String,
        signature: &str,
        hash: &str,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        super::models::EmissaryRelease::upsert_by_version(tag)
            .id(id)
            .manifest(raw)
            .signature(signature)
            .manifest_hash(hash)
            .or_ignore()
            .exec(&mut self.pool.clone())
            .await?;
        let row = sql::record(
            &mut self.pool.clone(),
            "SELECT id,manifest_hash FROM emissary_releases WHERE version=$1",
            params![tag],
            &[("id", Type::Uuid), ("manifest_hash", Type::String)],
        )
        .await?;
        ensure!(
            row.decode::<String>("manifest_hash")? == hash,
            "release version is immutable"
        );
        Ok(row.decode("id")?)
    }

    pub(crate) async fn next_emissary_update(
        &self,
        node: Uuid,
        session: Uuid,
    ) -> Result<Option<OtaOffer>> {
        let pool: Uuid = sql::scalar(
            &mut self.pool.clone(),
            "SELECT pool_id FROM nodes WHERE id=$1",
            params![node],
        )
        .await?;
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        let locked: bool = sql::scalar(
            &mut tx,
            "SELECT pg_try_advisory_xact_lock(hashtextextended($1,57))",
            params![pool.to_string()],
        )
        .await?;
        if !locked {
            tx.rollback().await?;
            return Ok(None);
        }
        let Some(n)=sql::optional_record(&mut tx, "SELECT n.*,coalesce(n.emissary_target_release,p.emissary_target_release) AS desired,p.emissary_update_concurrency,p.emissary_updates_paused FROM nodes n JOIN node_pools p ON p.id=n.pool_id WHERE n.id=$1 AND n.pool_id=$2 AND n.active_session=$3 AND NOT n.revoked AND n.certificate_expires_at>clock_timestamp() AND n.last_seen>now()-interval '30 seconds' FOR UPDATE OF n SKIP LOCKED", params![node, pool, session], &[("id", Type::Uuid),("name", Type::String),("os", Type::String),("arch", Type::String),("pool_id", Type::Uuid),("labels", Type::String),("capabilities", Type::String),("diagnostics", Type::String),("signing_profiles", Type::List(Box::new(Type::String))),("slots", Type::I32),("draining", Type::Bool),("revoked", Type::Bool),("certificate_hash", Type::String),("certificate_expires_at", Type::Timestamp),("active_session", Type::Uuid),("last_seen", Type::Timestamp),("created_at", Type::Timestamp),("emissary_target_release", Type::Uuid),("emissary_current_version", Type::String),("emissary_launcher_version", Type::String),("emissary_ota_status", Type::String),("ota_draining", Type::Bool),("desired", Type::Uuid),("emissary_update_concurrency", Type::I32),("emissary_updates_paused", Type::Bool)]).await? else{tx.rollback().await?;return Ok(None)};
        let mut update = sql::optional_record(
            &mut tx,
            "SELECT * FROM emissary_updates WHERE node_id=$1 AND finished_at IS NULL",
            params![node],
            &[
                ("id", Type::Uuid),
                ("generation", Type::I64),
                ("node_id", Type::Uuid),
                ("pool_id", Type::Uuid),
                ("release_id", Type::Uuid),
                ("previous_version", Type::String),
                ("status", Type::String),
                ("cancel_requested", Type::Bool),
                ("error", Type::String),
                ("retry_of", Type::Uuid),
                ("created_at", Type::Timestamp),
                ("updated_at", Type::Timestamp),
                ("finished_at", Type::Timestamp),
                ("last_command_at", Type::Timestamp),
            ],
        )
        .await?;
        if update.is_none() {
            let Some(target) = n.decode::<Option<Uuid>>("desired")? else {
                tx.rollback().await?;
                return Ok(None);
            };
            if n.decode::<bool>("emissary_updates_paused")?
                || !n
                    .decode::<Value>("capabilities")?
                    .as_array()
                    .is_some_and(|a| a.contains(&json!("emissary-ota-v1")))
            {
                tx.rollback().await?;
                return Ok(None);
            }
            let version: String = sql::scalar(
                &mut tx,
                "SELECT version FROM emissary_releases WHERE id=$1",
                params![target],
            )
            .await?;
            if n.decode::<Option<String>>("emissary_current_version")?
                .as_deref()
                == Some(&version)
            {
                tx.rollback().await?;
                return Ok(None);
            }
            let blocked:bool=sql::scalar(&mut tx, "SELECT EXISTS(SELECT 1 FROM emissary_updates WHERE node_id=$1 AND release_id=$2 AND status IN ('failed','rolled_back','canceled'))", params![node, target]).await?;
            if blocked {
                tx.rollback().await?;
                return Ok(None);
            }
            let count: i64 = sql::scalar(
                &mut tx,
                "SELECT count(*) FROM emissary_updates WHERE pool_id=$1 AND finished_at IS NULL",
                params![pool],
            )
            .await?;
            if count >= i64::from(n.decode::<i32>("emissary_update_concurrency")?) {
                tx.rollback().await?;
                return Ok(None);
            }
            let id = Uuid::new_v4();
            super::models::EmissaryUpdate::create()
                .id(id)
                .node_id(node)
                .pool_id(pool)
                .release_id(target)
                .previous_version(n.decode::<Option<String>>("emissary_current_version")?)
                .exec(&mut tx)
                .await?;
            sql::execute(
                &mut tx,
                "UPDATE nodes SET ota_draining=true WHERE id=$1",
                params![node],
            )
            .await?;
            update = sql::optional_record(
                &mut tx,
                "SELECT * FROM emissary_updates WHERE id=$1",
                params![id],
                &[
                    ("id", Type::Uuid),
                    ("generation", Type::I64),
                    ("node_id", Type::Uuid),
                    ("pool_id", Type::Uuid),
                    ("release_id", Type::Uuid),
                    ("previous_version", Type::String),
                    ("status", Type::String),
                    ("cancel_requested", Type::Bool),
                    ("error", Type::String),
                    ("retry_of", Type::Uuid),
                    ("created_at", Type::Timestamp),
                    ("updated_at", Type::Timestamp),
                    ("finished_at", Type::Timestamp),
                    ("last_command_at", Type::Timestamp),
                ],
            )
            .await?;
        }
        let u = update.unwrap();
        if u.decode::<Option<chrono::DateTime<chrono::Utc>>>("last_command_at")?
            .is_some_and(|t| chrono::Utc::now() - t < chrono::Duration::seconds(10))
        {
            tx.rollback().await?;
            return Ok(None);
        }
        let id: Uuid = u.decode("id")?;
        let r = sql::record(
            &mut tx,
            "SELECT * FROM emissary_releases WHERE id=$1",
            params![u.decode::<Uuid>("release_id")?],
            &[
                ("id", Type::Uuid),
                ("version", Type::String),
                ("manifest", Type::String),
                ("signature", Type::String),
                ("manifest_hash", Type::String),
                ("created_at", Type::Timestamp),
            ],
        )
        .await?;
        let raw: String = r.decode("manifest")?;
        let m: Value = serde_json::from_str(&raw)?;
        let node_os: String = n.decode("os")?;
        let node_arch: String = n.decode("arch")?;
        let a = m["assets"]
            .as_array()
            .context("invalid stored manifest")?
            .iter()
            .find(|a| {
                a["os"].as_str() == Some(node_os.as_str())
                    && a["arch"].as_str() == Some(node_arch.as_str())
            })
            .context("platform not present")?;
        let object_key = format!(
            "emissary-releases/{}/{}",
            r.decode::<String>("manifest_hash")?,
            a["name"].as_str().context("invalid asset")?
        );
        sql::execute(&mut tx, "UPDATE emissary_updates SET last_command_at=now(),status=CASE WHEN status='queued' THEN 'offered' ELSE status END WHERE id=$1", params![id]).await?;
        tx.commit().await?;
        Ok(Some(OtaOffer {
            id,
            generation: u.decode("generation")?,
            cancel: u.decode("cancel_requested")?,
            manifest: raw,
            signature: r.decode("signature")?,
            object_key,
        }))
    }

    pub(crate) async fn report_emissary_update(
        &self,
        node: Uuid,
        session: Uuid,
        payload: &Value,
    ) -> Result<bool> {
        let Some(status) = payload.get("ota").filter(|s| s.is_object()) else {
            return Ok(false);
        };
        let pool: Uuid = sql::scalar(
            &mut self.pool.clone(),
            "SELECT pool_id FROM nodes WHERE id=$1",
            params![node],
        )
        .await?;
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        sql::execute(
            &mut tx,
            "SELECT pg_advisory_xact_lock(hashtextextended($1,57))",
            params![pool.to_string()],
        )
        .await?;
        let valid:Option<Uuid>=sql::optional_scalar(&mut tx, "SELECT id FROM nodes WHERE id=$1 AND active_session=$2 AND NOT revoked AND certificate_expires_at>clock_timestamp() FOR UPDATE", params![node, session]).await?;
        ensure!(valid.is_some(), "node session revoked or superseded");
        let still_valid:bool=sql::scalar(&mut tx, "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND active_session=$2 AND NOT revoked AND certificate_expires_at>clock_timestamp())", params![node, session]).await?;
        ensure!(
            still_valid,
            "node certificate expired while waiting for its lock"
        );
        let runtime = payload["diagnostics"]["emissary_version"]
            .as_str()
            .unwrap_or("");
        sql::execute(&mut tx, "UPDATE nodes SET emissary_current_version=$2,emissary_launcher_version=$3,emissary_ota_status=$4 WHERE id=$1", params![node, runtime, status["launcher_version"].as_str(), status]).await?;
        let mut health = true;
        if let Some(id) = status["update_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        && let Some(u)=sql::optional_record(&mut tx, "SELECT u.*,r.version FROM emissary_updates u JOIN emissary_releases r ON r.id=u.release_id WHERE u.id=$1 AND u.node_id=$2 AND u.finished_at IS NULL", params![id, node], &[("id", Type::Uuid),("generation", Type::I64),("node_id", Type::Uuid),("pool_id", Type::Uuid),("release_id", Type::Uuid),("previous_version", Type::String),("status", Type::String),("cancel_requested", Type::Bool),("error", Type::String),("retry_of", Type::Uuid),("created_at", Type::Timestamp),("updated_at", Type::Timestamp),("finished_at", Type::Timestamp),("last_command_at", Type::Timestamp),("version", Type::String)]).await? {
            let phase=status["phase"].as_str().unwrap_or("");
            let terminal=["succeeded","failed","rolled_back","canceled"].contains(&phase);
            if phase=="activating"||phase=="succeeded" {
                health=runtime==u.decode::<String>("version")?
                    || (phase=="activating"&&status["activation_pending"]==true&&Some(runtime)==u.decode::<Option<String>>("previous_version")?.as_deref());
            }
            if ["downloading","draining","activating","succeeded","failed","rolled_back","canceled"].contains(&phase)&&health {
                sql::execute(&mut tx, "UPDATE emissary_updates SET status=$2,error=$3,updated_at=now(),finished_at=CASE WHEN $4 THEN now() ELSE NULL END WHERE id=$1", params![id, phase, status["error"].as_str().map(|s|s.chars().take(1000).collect::<String>()), terminal]).await?;
                if terminal {
                    sql::execute(&mut tx, "UPDATE nodes SET ota_draining=false WHERE id=$1", params![node]).await?;
                    if phase=="failed"||phase=="rolled_back" {sql::execute(&mut tx, "UPDATE node_pools SET emissary_updates_paused=true WHERE id=$1", params![u.decode::<Uuid>("pool_id")?]).await?;}
                    super::models::AuditEvent::create().actor("emissary").action("emissary_update_finished").resource_id(id.to_string()).details(toasty::Json((json!({"node_id":node,"status":phase,"error":status["error"]})).clone())).exec(&mut tx).await?;
                }
            }
    }
        tx.commit().await?;
        Ok(health)
    }
}
