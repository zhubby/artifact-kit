//! Authentication persistence and atomic registration.
use artifactory_database::{params, sql};
use toasty::stmt::Type;

use super::{
    ControlStore,
    models::{Session, User},
};
use crate::{
    auth::Auth,
    error::{ApiError, Result},
};

use uuid::Uuid;

/// Credentials needed by the password verifier; never serialized to a response.
pub struct LoginAccount {
    pub id: Uuid,
    pub password_hash: String,
    pub role: String,
}

impl ControlStore {
    pub async fn authenticate_token(&self, hash: &str) -> Result<Option<Auth>> {
        let row = sql::optional_record(&mut self.pool.clone(), "SELECT u.id,u.username,u.role,t.project_ids,t.scopes FROM api_tokens t JOIN users u ON u.id=t.user_id WHERE t.token_hash=$1 AND NOT t.revoked AND t.expires_at>now()", params![hash], &[("id", Type::Uuid),("username", Type::String),("role", Type::String),("project_ids", Type::List(Box::new(Type::Uuid))),("scopes", Type::List(Box::new(Type::String)))]).await?;
        Ok(row
            .map(|row| {
                Ok::<_, artifactory_database::Error>(Auth {
                    user_id: row.decode("id")?,
                    username: row.decode("username")?,
                    role: row.decode("role")?,
                    token_projects: Some(row.decode("project_ids")?),
                    token_scopes: row.decode("scopes")?,
                })
            })
            .transpose()?)
    }

    pub async fn authenticate_session(&self, hash: &str) -> Result<Option<Auth>> {
        let row = sql::optional_record(&mut self.pool.clone(), "SELECT u.id,u.username,u.role FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.token_hash=$1 AND s.expires_at>now()", params![hash], &[("id", Type::Uuid),("username", Type::String),("role", Type::String)]).await?;
        Ok(row
            .map(|row| {
                Ok::<_, artifactory_database::Error>(Auth {
                    user_id: row.decode("id")?,
                    username: row.decode("username")?,
                    role: row.decode("role")?,
                    token_projects: None,
                    token_scopes: vec![],
                })
            })
            .transpose()?)
    }

    pub async fn reserve_registration(&self) -> Result<()> {
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        sql::execute(
            &mut tx,
            "SELECT pg_advisory_xact_lock(hashtextextended('artifactory:registration',0))",
            params![],
        )
        .await?;
        let (attempts, retry_after): (i64, i64) = sql::tuple(&mut tx, "SELECT count(*),coalesce(ceil(extract(epoch FROM (min(created_at)+interval '5 minutes'-statement_timestamp())))::bigint,0) FROM audit_events WHERE action='registration_attempt' AND created_at>statement_timestamp()-interval '5 minutes'", params![]).await?;
        if attempts >= 10 {
            tx.rollback().await?;
            return Err(ApiError::TooManyRequests(retry_after.max(1) as u64));
        }
        sql::execute(&mut tx, "INSERT INTO audit_events(actor,action,resource_id,created_at) VALUES('anonymous','registration_attempt','registration',clock_timestamp())", params![]).await?;
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
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        super::models::User::create()
            .id(id)
            .username(username)
            .password_hash(password_hash)
            .role("viewer")
            .exec(&mut tx)
            .await?;
        let expires: jiff::Timestamp =
            sql::scalar(&mut tx, "SELECT now()+interval '24 hours'", params![]).await?;
        Session::create()
            .token_hash(session_hash)
            .user_id(id)
            .expires_at(expires)
            .exec(&mut tx)
            .await?;
        super::models::AuditEvent::create()
            .actor(id.to_string())
            .action("user_registered")
            .resource_id(id.to_string())
            .details(toasty::Json(serde_json::json!({"role": "viewer"})))
            .exec(&mut tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn recent_login_failures(&self, username: &str) -> Result<i64> {
        Ok(sql::scalar(&mut self.pool.clone(), "SELECT count(*) FROM audit_events WHERE action='login_failed' AND resource_id=$1 AND created_at>now()-interval '5 minutes'", params![username]).await?)
    }

    pub async fn login_account(&self, username: &str) -> Result<Option<LoginAccount>> {
        Ok(User::filter_by_username(username)
            .first()
            .exec(&mut self.pool.clone())
            .await?
            .map(|user| LoginAccount {
                id: user.id,
                password_hash: user.password_hash,
                role: user.role,
            }))
    }

    pub async fn create_session(&self, hash: &str, user: Uuid) -> Result<()> {
        let mut db = self.pool.clone();
        let mut tx = db.transaction().await?;
        let expires: jiff::Timestamp =
            sql::scalar(&mut tx, "SELECT now()+interval '24 hours'", params![]).await?;
        Session::create()
            .token_hash(hash)
            .user_id(user)
            .expires_at(expires)
            .exec(&mut tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn remove_session(&self, hash: &str) -> Result<()> {
        super::models::Session::filter(super::models::Session::fields().token_hash().eq(hash))
            .delete()
            .exec(&mut self.pool.clone())
            .await?;
        Ok(())
    }
}
