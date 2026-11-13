//! Authentication persistence and atomic registration.

use super::ControlStore;
use crate::{
    auth::Auth,
    error::{ApiError, Result},
};
use sqlx::Row;
use uuid::Uuid;

/// Credentials needed by the password verifier; never serialized to a response.
pub struct LoginAccount {
    pub id: Uuid,
    pub password_hash: String,
    pub role: String,
}

impl ControlStore {
    pub async fn authenticate_token(&self, hash: &str) -> Result<Option<Auth>> {
        let row = sqlx::query("SELECT u.id,u.username,u.role,t.project_ids,t.scopes FROM api_tokens t JOIN users u ON u.id=t.user_id WHERE t.token_hash=$1 AND NOT t.revoked AND t.expires_at>now()")
            .bind(hash).fetch_optional(&self.pool).await?;
        Ok(row.map(|row| Auth {
            user_id: row.get("id"),
            username: row.get("username"),
            role: row.get("role"),
            token_projects: Some(row.get("project_ids")),
            token_scopes: row.get("scopes"),
        }))
    }

    pub async fn authenticate_session(&self, hash: &str) -> Result<Option<Auth>> {
        let row = sqlx::query("SELECT u.id,u.username,u.role FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.token_hash=$1 AND s.expires_at>now()")
            .bind(hash).fetch_optional(&self.pool).await?;
        Ok(row.map(|row| Auth {
            user_id: row.get("id"),
            username: row.get("username"),
            role: row.get("role"),
            token_projects: None,
            token_scopes: vec![],
        }))
    }

    pub async fn reserve_registration(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('artifactory:registration',0))")
            .execute(&mut *tx)
            .await?;
        let (attempts, retry_after): (i64, i64) = sqlx::query_as(
            "SELECT count(*),coalesce(ceil(extract(epoch FROM (min(created_at)+interval '5 minutes'-statement_timestamp())))::bigint,0) FROM audit_events WHERE action='registration_attempt' AND created_at>statement_timestamp()-interval '5 minutes'",
        ).fetch_one(&mut *tx).await?;
        if attempts >= 10 {
            tx.rollback().await?;
            return Err(ApiError::TooManyRequests(retry_after.max(1) as u64));
        }
        sqlx::query("INSERT INTO audit_events(actor,action,resource_id,created_at) VALUES('anonymous','registration_attempt','registration',clock_timestamp())")
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn register_viewer(
        &self,
        id: Uuid,
        username: &str,
        password_hash: &str,
        session_hash: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO users(id,username,password_hash,role) VALUES($1,$2,$3,'viewer')")
            .bind(id)
            .bind(username)
            .bind(password_hash)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO sessions(token_hash,user_id,expires_at) VALUES($1,$2,now()+interval '24 hours')")
            .bind(session_hash).bind(id).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO audit_events(actor,action,resource_id,details) VALUES($1,'user_registered',$1,'{\"role\":\"viewer\"}')")
            .bind(id.to_string()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn recent_login_failures(&self, username: &str) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='login_failed' AND resource_id=$1 AND created_at>now()-interval '5 minutes'")
            .bind(username).fetch_one(&self.pool).await?)
    }

    pub async fn login_account(&self, username: &str) -> Result<Option<LoginAccount>> {
        Ok(
            sqlx::query("SELECT id,password_hash,role FROM users WHERE username=$1")
                .bind(username)
                .fetch_optional(&self.pool)
                .await?
                .map(|row| LoginAccount {
                    id: row.get("id"),
                    password_hash: row.get("password_hash"),
                    role: row.get("role"),
                }),
        )
    }

    pub async fn create_session(&self, hash: &str, user: Uuid) -> Result<()> {
        sqlx::query("INSERT INTO sessions(token_hash,user_id,expires_at) VALUES($1,$2,now()+interval '24 hours')")
            .bind(hash).bind(user).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn remove_session(&self, hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE token_hash=$1")
            .bind(hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
