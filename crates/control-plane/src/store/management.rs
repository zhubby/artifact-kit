//! Management queries and atomic node updates.
use super::ControlStore;
use super::dto;
use crate::error::{ApiError, Result};
use artifactory_database::{params, sql};
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
    pub async fn overview(&self) -> Result<dto::Overview> {
        Ok(sql::scalar(&mut self.pool.clone(), "SELECT jsonb_build_object('projects',(SELECT count(*) FROM projects),'nodes_registered',(SELECT count(*) FROM nodes),'nodes_online',(SELECT count(*) FROM nodes WHERE NOT revoked AND last_seen>now()-interval '30 seconds'),'running',(SELECT count(*) FROM jobs WHERE status IN ('leased','running')),'queued',(SELECT count(*) FROM jobs WHERE status='queued'),'releases',(SELECT count(*) FROM releases),'builds_today',(SELECT count(*) FROM builds WHERE created_at>now()-interval '24 hours'))", params![]).await?)
    }

    pub async fn users(&self) -> Result<Vec<dto::UserSummary>> {
        Ok(sql::scalars(&mut self.pool.clone(), "SELECT jsonb_build_object('id',id,'username',username,'role',role,'created_at',created_at) FROM users ORDER BY created_at", params![]).await?)
    }

    pub async fn create_user(
        &self,
        id: Uuid,
        username: &str,
        password: &str,
        role: &str,
    ) -> Result<()> {
        super::models::User::create()
            .id(id)
            .username(username)
            .password_hash(password)
            .role(role)
            .exec(&mut self.pool.clone())
            .await?;
        Ok(())
    }

    pub async fn tokens(&self, user_id: Uuid) -> Result<Vec<dto::TokenSummary>> {
        Ok(sql::scalars(&mut self.pool.clone(), "SELECT to_jsonb(t)-'token_hash' FROM api_tokens t WHERE user_id=$1 ORDER BY created_at DESC", params![user_id]).await?)
    }

    pub async fn project_exists(&self, project: Uuid) -> Result<bool> {
        Ok(sql::scalar(
            &mut self.pool.clone(),
            "SELECT EXISTS(SELECT 1 FROM projects WHERE id=$1)",
            params![project],
        )
        .await?)
    }

    pub async fn create_token(
        &self,
        id: Uuid,
        user_id: Uuid,
        input: NewToken,
        hash: String,
    ) -> Result<()> {
        super::models::ApiToken::create()
            .id(id)
            .user_id(user_id)
            .name(input.name)
            .token_hash(hash)
            .project_ids(input.project_ids)
            .scopes(input.scopes)
            .expires_at(sql::timestamp(Utc::now() + Duration::days(input.days))?)
            .exec(&mut self.pool.clone())
            .await?;
        Ok(())
    }

    pub async fn revoke_token(&self, id: Uuid, user_id: Uuid) -> Result<()> {
        super::models::ApiToken::filter(
            super::models::ApiToken::fields()
                .id()
                .eq(id)
                .and(super::models::ApiToken::fields().user_id().eq(user_id)),
        )
        .update()
        .revoked(true)
        .exec(&mut self.pool.clone())
        .await?;
        Ok(())
    }

    pub async fn pools(&self) -> Result<Vec<dto::PoolSummary>> {
        Ok(sql::scalars(
            &mut self.pool.clone(),
            "SELECT to_jsonb(p) FROM node_pools p ORDER BY name",
            params![],
        )
        .await?)
    }

    pub async fn create_pool(&self, id: Uuid, name: &str) -> Result<()> {
        super::models::NodePool::create()
            .id(id)
            .name(name)
            .exec(&mut self.pool.clone())
            .await?;
        Ok(())
    }

    pub async fn enrollment_token(&self, id: Uuid, hash: String, pool_id: Uuid) -> Result<()> {
        sql::execute(&mut self.pool.clone(), "INSERT INTO enrollment_tokens(id,token_hash,pool_id,expires_at) VALUES($1,$2,$3,now()+interval '15 minutes')", params![id, hash, pool_id]).await?;
        Ok(())
    }

    pub async fn nodes(&self, limit: i64, offset: i64, q: &str) -> Result<Vec<dto::NodeSummary>> {
        // Project before encoding so list requests never deserialize large metadata values.
        Ok(sql::scalars(&mut self.pool.clone(), r#"SELECT to_jsonb(summary) FROM (
            SELECT n.id,n.name,n.os,n.arch,n.pool_id,n.labels,n.capabilities,n.diagnostics,
                n.signing_profiles,n.slots,n.draining,n.revoked,n.certificate_expires_at,
                n.last_seen,n.created_at,n.emissary_target_release,n.emissary_current_version,
                n.emissary_launcher_version,n.emissary_ota_status,n.ota_draining,
                coalesce(n.emissary_target_release,p.emissary_target_release) AS emissary_desired_release,
                p.emissary_updates_paused AS emissary_pool_paused,p.name AS pool_name,
                NOT n.revoked AND n.last_seen>now()-interval '30 seconds' AS online,
                (SELECT count(*) FROM attempts a WHERE a.node_id=n.id
                    AND a.status IN ('leased','running') AND a.lease_expires_at>now()) AS active_jobs
            FROM nodes n JOIN node_pools p ON p.id=n.pool_id
            WHERE strpos(lower(n.name||' '||n.os||' '||p.name),lower($3))>0
            ORDER BY n.created_at DESC,n.id DESC LIMIT $1 OFFSET $2
        ) summary"#, params![limit, offset, q]).await?)
    }

    pub async fn update_node(&self, id: Uuid, input: &NodeUpdate) -> Result<bool> {
        let mut transaction_db = self.pool.clone();
        let mut transaction = transaction_db.transaction().await?;
        sql::execute(
            &mut transaction,
            "SELECT id FROM nodes WHERE id=$1 FOR UPDATE",
            params![id],
        )
        .await?;
        let changing_pool_while_updating: bool = sql::scalar(
            &mut transaction,
            "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND ota_draining AND pool_id<>$2)",
            params![id, input.pool_id],
        )
        .await?;
        if changing_pool_while_updating {
            return Err(ApiError::Conflict(
                "wait for the active Emissary update before changing its pool".into(),
            ));
        }
        let revoked=sql::optional_scalar::<bool>(&mut transaction, "UPDATE nodes SET draining=$2,revoked=revoked OR $3,slots=$4,labels=$5,signing_profiles=$6,pool_id=$7 WHERE id=$1 RETURNING revoked", params![id, input.draining, input.revoked, input.slots, &input.labels, &input.signing_profiles, input.pool_id]).await?.ok_or(ApiError::NotFound)?;
        transaction.commit().await?;
        Ok(revoked)
    }

    pub async fn credentials(&self) -> Result<Vec<dto::CredentialSummary>> {
        Ok(sql::scalars(
            &mut self.pool.clone(),
            "SELECT to_jsonb(c)-'ciphertext' FROM credentials c ORDER BY created_at DESC",
            params![],
        )
        .await?)
    }

    pub async fn create_credential(
        &self,
        id: Uuid,
        name: String,
        kind: String,
        ciphertext: String,
    ) -> Result<()> {
        self.source
            .create_control_credential(id, name, kind, ciphertext)
            .await?;
        Ok(())
    }

    pub async fn audit_events(&self) -> Result<Vec<dto::AuditEvent>> {
        Ok(sql::scalars(
            &mut self.pool.clone(),
            "SELECT to_jsonb(a) FROM audit_events a ORDER BY id DESC LIMIT 200",
            params![],
        )
        .await?)
    }
}
