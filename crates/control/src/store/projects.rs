//! Project persistence and source configuration projections.
use super::dto;
use artifactory_database::{params, sql};

use super::ControlStore;
use crate::{error::Result, source_cache::CredentialBinding};
use serde::Deserialize;

use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectInput {
    #[serde(default)]
    pub planner_backend: Option<String>,
    pub name: String,
    pub repository_url: String,
    pub credential_id: Option<Uuid>,
    #[serde(default = "default_pools")]
    pub pool_ids: Vec<Uuid>,
    #[serde(default)]
    pub signing_profiles: Vec<String>,
    #[serde(default)]
    pub model_profile_id: Option<Uuid>,
    #[serde(default)]
    pub build_context: String,
    #[serde(default)]
    pub source_credentials: Vec<CredentialBinding>,
}

fn default_pools() -> Vec<Uuid> {
    vec![Uuid::from_u128(1)]
}

/// Source routing information independent of the database representation.
pub struct ProjectSource {
    pub repository_url: String,
    pub credential_id: Option<Uuid>,
    pub planner_backend: String,
}

impl ControlStore {
    pub async fn projects(&self, visible: Option<Vec<Uuid>>) -> Result<Vec<dto::ProjectSummary>> {
        Ok(sql::scalars(&mut self.pool.clone(), "SELECT to_jsonb(p) || jsonb_build_object('build_count',(SELECT count(*) FROM builds b WHERE b.project_id=p.id),'last_build',(SELECT jsonb_build_object('id',b.id,'status',b.status,'created_at',b.created_at) FROM builds b WHERE b.project_id=p.id ORDER BY created_at DESC LIMIT 1)) FROM projects p WHERE ($1::uuid[] IS NULL OR p.id=ANY($1)) ORDER BY p.created_at DESC", params![visible]).await?)
    }

    pub async fn project(&self, id: Uuid) -> Result<dto::Project> {
        Ok(sql::scalar(
            &mut self.pool.clone(),
            "SELECT to_jsonb(p) FROM projects p WHERE id=$1",
            params![id],
        )
        .await?)
    }

    pub async fn project_source(&self, id: Uuid) -> Result<ProjectSource> {
        let row = super::models::Project::get_by_id(&mut self.pool.clone(), id).await?;
        Ok(ProjectSource {
            repository_url: row.repository_url,
            credential_id: row.credential_id,
            planner_backend: row.planner_backend,
        })
    }

    pub async fn pool_count(&self, ids: &[Uuid]) -> Result<i64> {
        Ok(sql::scalar(
            &mut self.pool.clone(),
            "SELECT count(*) FROM node_pools WHERE id=ANY($1)",
            params![ids],
        )
        .await?)
    }

    pub async fn create_project(&self, id: Uuid, input: &ProjectInput) -> Result<()> {
        super::models::Project::create()
            .id(id)
            .name(&input.name)
            .repository_url(&input.repository_url)
            .credential_id(input.credential_id)
            .pool_ids(input.pool_ids.clone())
            .signing_profiles(input.signing_profiles.clone())
            .model_profile_id(input.model_profile_id)
            .build_context(&input.build_context)
            .source_credentials(toasty::Json(
                serde_json::to_value(&input.source_credentials).map_err(anyhow::Error::from)?,
            ))
            .planner_backend(input.planner_backend.as_deref().unwrap_or("poll"))
            .exec(&mut self.pool.clone())
            .await?;
        Ok(())
    }

    pub async fn update_project(&self, id: Uuid, input: &ProjectInput) -> Result<()> {
        let update = super::models::Project::filter_by_id(id)
            .update()
            .name(&input.name)
            .repository_url(&input.repository_url)
            .credential_id(input.credential_id)
            .pool_ids(input.pool_ids.clone())
            .signing_profiles(input.signing_profiles.clone())
            .model_profile_id(input.model_profile_id)
            .build_context(&input.build_context)
            .source_credentials(toasty::Json(
                serde_json::to_value(&input.source_credentials).map_err(anyhow::Error::from)?,
            ));
        let update = if let Some(backend) = &input.planner_backend {
            update.planner_backend(backend)
        } else {
            update
        };
        update.exec(&mut self.pool.clone()).await?;
        Ok(())
    }
}
