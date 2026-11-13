use super::ControlStore;
use crate::error::{ApiError, Result};
use uuid::Uuid;
impl ControlStore {
    pub(crate) async fn credential_ciphertext(&self, id: Uuid) -> anyhow::Result<String> {
        let encrypted: String =
            sqlx::query_scalar("SELECT ciphertext FROM credentials WHERE id=$1")
                .bind(id)
                .fetch_one(&self.pool)
                .await?;
        Ok(encrypted)
    }
    pub(crate) async fn node_exists(&self, id: Uuid) -> Result<bool> {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1)")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        Ok(exists)
    }
    pub(crate) async fn attempt_project(&self, id: Uuid) -> Result<Uuid> {
        let project:Uuid=sqlx::query_scalar("SELECT b.project_id FROM attempts a JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE a.id=$1")
        .bind(id).fetch_optional(&self.pool).await?.ok_or(ApiError::NotFound)?;
        Ok(project)
    }
}
