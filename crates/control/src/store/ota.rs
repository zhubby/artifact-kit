//! OTA release import and serialized update coordination.
use super::ControlStore;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sqlx::Row;
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
        if let Some(row) =
            sqlx::query("SELECT id,manifest_hash FROM emissary_releases WHERE version=$1")
                .bind(tag)
                .fetch_optional(&self.pool)
                .await?
        {
            ensure!(
                row.get::<String, _>("manifest_hash") == hash,
                "release version is immutable"
            );
            return Ok(Some(row.get("id")));
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
        sqlx::query("INSERT INTO emissary_releases(id,version,manifest,signature,manifest_hash) VALUES($1,$2,$3,$4,$5) ON CONFLICT(version) DO NOTHING").bind(id).bind(tag).bind(raw).bind(signature).bind(hash).execute(&self.pool).await?;
        let row = sqlx::query("SELECT id,manifest_hash FROM emissary_releases WHERE version=$1")
            .bind(tag)
            .fetch_one(&self.pool)
            .await?;
        ensure!(
            row.get::<String, _>("manifest_hash") == hash,
            "release version is immutable"
        );
        Ok(row.get("id"))
    }

    pub(crate) async fn next_emissary_update(
        &self,
        node: Uuid,
        session: Uuid,
    ) -> Result<Option<OtaOffer>> {
        let pool: Uuid = sqlx::query_scalar("SELECT pool_id FROM nodes WHERE id=$1")
            .bind(node)
            .fetch_one(&self.pool)
            .await?;
        let mut tx = self.pool.begin().await?;
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
        let object_key = format!(
            "emissary-releases/{}/{}",
            r.get::<String, _>("manifest_hash"),
            a["name"].as_str().context("invalid asset")?
        );
        sqlx::query("UPDATE emissary_updates SET last_command_at=now(),status=CASE WHEN status='queued' THEN 'offered' ELSE status END WHERE id=$1").bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Some(OtaOffer {
            id,
            generation: u.get("generation"),
            cancel: u.get("cancel_requested"),
            manifest: raw,
            signature: r.get("signature"),
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
        let pool: Uuid = sqlx::query_scalar("SELECT pool_id FROM nodes WHERE id=$1")
            .bind(node)
            .fetch_one(&self.pool)
            .await?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,57))")
            .bind(pool.to_string())
            .execute(&mut *tx)
            .await?;
        let valid:Option<Uuid>=sqlx::query_scalar("SELECT id FROM nodes WHERE id=$1 AND active_session=$2 AND NOT revoked AND certificate_expires_at>clock_timestamp() FOR UPDATE").bind(node).bind(session).fetch_optional(&mut *tx).await?;
        ensure!(valid.is_some(), "node session revoked or superseded");
        let still_valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND active_session=$2 AND NOT revoked AND certificate_expires_at>clock_timestamp())").bind(node).bind(session).fetch_one(&mut *tx).await?;
        ensure!(
            still_valid,
            "node certificate expired while waiting for its lock"
        );
        let runtime = payload["diagnostics"]["emissary_version"]
            .as_str()
            .unwrap_or("");
        sqlx::query("UPDATE nodes SET emissary_current_version=$2,emissary_launcher_version=$3,emissary_ota_status=$4 WHERE id=$1").bind(node).bind(runtime).bind(status["launcher_version"].as_str()).bind(status).execute(&mut *tx).await?;
        let mut health = true;
        if let Some(id) = status["update_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
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
}
