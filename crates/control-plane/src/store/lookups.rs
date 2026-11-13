use super::ControlStore;
use crate::error::{ApiError, Result};
use artifactory_database::{params, sql};
use uuid::Uuid;
impl ControlStore {
    pub(crate) async fn credential_ciphertext(&self, id: Uuid) -> anyhow::Result<String> {
        self.source.credential_ciphertext(id).await
    }
    pub(crate) async fn node_exists(&self, id: Uuid) -> Result<bool> {
        let exists: bool = sql::scalar(
            &mut self.pool.clone(),
            "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1)",
            params![id],
        )
        .await?;
        Ok(exists)
    }
    pub(crate) async fn attempt_project(&self, id: Uuid) -> Result<Uuid> {
        let project:Uuid=sql::optional_scalar(&mut self.pool.clone(), "SELECT b.project_id FROM attempts a JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE a.id=$1", params![id]).await?.ok_or(ApiError::NotFound)?;
        Ok(project)
    }
}

impl ControlStore {
    pub(crate) async fn source_refs_project(
        &self,
        project: Uuid,
    ) -> anyhow::Result<(String, Option<Uuid>)> {
        Ok(sql::tuple(
            &mut self.pool.clone(),
            "SELECT repository_url,credential_id FROM projects WHERE id=$1",
            params![project],
        )
        .await?)
    }
}
