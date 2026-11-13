//! Management queries and atomic node updates.
use super::ControlStore;
use crate::error::{ApiError, Result};
use chrono::{Duration, Utc};
use serde_json::Value;
use uuid::Uuid;

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewToken {
    pub(crate) name: String,
    pub(crate) project_ids: Vec<Uuid>,
    pub(crate) scopes: Vec<String>,
    pub(crate) days: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeUpdate {
    pub(crate) draining: bool,
    pub(crate) revoked: bool,
    pub(crate) slots: i32,
    pub(crate) labels: Value,
    pub(crate) signing_profiles: Vec<String>,
    pub(crate) pool_id: Uuid,
}

impl ControlStore {
    pub async fn overview(&self) -> Result<Value> {
        Ok(sqlx::query_scalar("SELECT jsonb_build_object('projects',(SELECT count(*) FROM projects),'nodes_online',(SELECT count(*) FROM nodes WHERE NOT revoked AND last_seen>now()-interval '30 seconds'),'running',(SELECT count(*) FROM jobs WHERE status IN ('leased','running')),'queued',(SELECT count(*) FROM jobs WHERE status='queued'),'releases',(SELECT count(*) FROM releases),'builds_today',(SELECT count(*) FROM builds WHERE created_at>now()-interval '24 hours'))")
        .fetch_one(&self.pool).await?)
    }

    pub async fn users(&self) -> Result<Vec<Value>> {
        Ok(sqlx::query_scalar("SELECT jsonb_build_object('id',id,'username',username,'role',role,'created_at',created_at) FROM users ORDER BY created_at").fetch_all(&self.pool).await?)
    }

    pub async fn create_user(
        &self,
        id: Uuid,
        username: &str,
        password: &str,
        role: &str,
    ) -> Result<()> {
        sqlx::query("INSERT INTO users(id,username,password_hash,role) VALUES($1,$2,$3,$4)")
            .bind(id)
            .bind(username)
            .bind(password)
            .bind(role)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn tokens(&self, user_id: Uuid) -> Result<Vec<Value>> {
        Ok(sqlx::query_scalar("SELECT to_jsonb(t)-'token_hash' FROM api_tokens t WHERE user_id=$1 ORDER BY created_at DESC").bind(user_id).fetch_all(&self.pool).await?)
    }

    pub async fn project_exists(&self, project: Uuid) -> Result<bool> {
        Ok(
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM projects WHERE id=$1)")
                .bind(project)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub async fn create_token(
        &self,
        id: Uuid,
        user_id: Uuid,
        input: NewToken,
        hash: String,
    ) -> Result<()> {
        sqlx::query("INSERT INTO api_tokens(id,user_id,name,token_hash,project_ids,scopes,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(id).bind(user_id).bind(input.name).bind(hash).bind(input.project_ids).bind(input.scopes).bind(Utc::now()+Duration::days(input.days)).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn revoke_token(&self, id: Uuid, user_id: Uuid) -> Result<()> {
        sqlx::query("UPDATE api_tokens SET revoked=true WHERE id=$1 AND user_id=$2")
            .bind(id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn pools(&self) -> Result<Vec<Value>> {
        Ok(
            sqlx::query_scalar("SELECT to_jsonb(p) FROM node_pools p ORDER BY name")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn create_pool(&self, id: Uuid, name: &str) -> Result<()> {
        sqlx::query("INSERT INTO node_pools(id,name) VALUES($1,$2)")
            .bind(id)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn enrollment_token(&self, id: Uuid, hash: String, pool_id: Uuid) -> Result<()> {
        sqlx::query("INSERT INTO enrollment_tokens(id,token_hash,pool_id,expires_at) VALUES($1,$2,$3,now()+interval '15 minutes')").bind(id).bind(hash).bind(pool_id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn nodes(&self) -> Result<Vec<Value>> {
        Ok(sqlx::query_scalar("SELECT (to_jsonb(n)-'certificate_hash'-'active_session') || jsonb_build_object('emissary_desired_release',coalesce(n.emissary_target_release,p.emissary_target_release),'emissary_pool_paused',p.emissary_updates_paused,'pool_name',p.name,'online',NOT n.revoked AND n.last_seen>now()-interval '30 seconds','active_jobs',(SELECT count(*) FROM attempts a WHERE a.node_id=n.id AND a.status IN ('leased','running') AND a.lease_expires_at>now())) FROM nodes n JOIN node_pools p ON p.id=n.pool_id ORDER BY n.created_at DESC").fetch_all(&self.pool).await?)
    }

    pub async fn update_node(&self, id: Uuid, input: &NodeUpdate) -> Result<bool> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        let changing_pool_while_updating: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND ota_draining AND pool_id<>$2)",
        )
        .bind(id)
        .bind(input.pool_id)
        .fetch_one(&mut *transaction)
        .await?;
        if changing_pool_while_updating {
            return Err(ApiError::Conflict(
                "wait for the active Emissary update before changing its pool".into(),
            ));
        }
        let revoked=sqlx::query_scalar::<_,bool>("UPDATE nodes SET draining=$2,revoked=revoked OR $3,slots=$4,labels=$5,signing_profiles=$6,pool_id=$7 WHERE id=$1 RETURNING revoked").bind(id).bind(input.draining).bind(input.revoked).bind(input.slots).bind(&input.labels).bind(&input.signing_profiles).bind(input.pool_id).fetch_optional(&mut *transaction).await?.ok_or(ApiError::NotFound)?;
        transaction.commit().await?;
        Ok(revoked)
    }

    pub async fn credentials(&self) -> Result<Vec<Value>> {
        Ok(sqlx::query_scalar(
            "SELECT to_jsonb(c)-'ciphertext' FROM credentials c ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn create_credential(
        &self,
        id: Uuid,
        name: String,
        kind: String,
        ciphertext: String,
    ) -> Result<()> {
        sqlx::query("INSERT INTO credentials(id,name,kind,ciphertext) VALUES($1,$2,$3,$4)")
            .bind(id)
            .bind(name)
            .bind(kind)
            .bind(ciphertext)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn audit_events(&self) -> Result<Vec<Value>> {
        Ok(
            sqlx::query_scalar("SELECT to_jsonb(a) FROM audit_events a ORDER BY id DESC LIMIT 200")
                .fetch_all(&self.pool)
                .await?,
        )
    }
}
