//! Project persistence and source configuration projections.

use super::ControlStore;
use crate::{error::Result, source_cache::CredentialBinding};
use serde::Deserialize;
use serde_json::Value;
use sqlx::Row;
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
    pub async fn projects(&self, visible: Option<Vec<Uuid>>) -> Result<Vec<Value>> {
        Ok(sqlx::query_scalar("SELECT to_jsonb(p) || jsonb_build_object('build_count',(SELECT count(*) FROM builds b WHERE b.project_id=p.id),'last_build',(SELECT jsonb_build_object('id',b.id,'status',b.status,'created_at',b.created_at) FROM builds b WHERE b.project_id=p.id ORDER BY created_at DESC LIMIT 1)) FROM projects p WHERE ($1::uuid[] IS NULL OR p.id=ANY($1)) ORDER BY p.created_at DESC")
            .bind(visible).fetch_all(&self.pool).await?)
    }

    pub async fn project(&self, id: Uuid) -> Result<Value> {
        Ok(
            sqlx::query_scalar("SELECT to_jsonb(p) FROM projects p WHERE id=$1")
                .bind(id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub async fn project_source(&self, id: Uuid) -> Result<ProjectSource> {
        let row = sqlx::query(
            "SELECT repository_url,credential_id,planner_backend FROM projects WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(ProjectSource {
            repository_url: row.get("repository_url"),
            credential_id: row.get("credential_id"),
            planner_backend: row.get("planner_backend"),
        })
    }

    pub async fn pool_count(&self, ids: &[Uuid]) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT count(*) FROM node_pools WHERE id=ANY($1)")
                .bind(ids)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub async fn create_project(&self, id: Uuid, input: &ProjectInput) -> Result<()> {
        sqlx::query("INSERT INTO projects(id,name,repository_url,credential_id,pool_ids,signing_profiles,model_profile_id,build_context,source_credentials,planner_backend) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
            .bind(id).bind(&input.name).bind(&input.repository_url).bind(input.credential_id)
            .bind(&input.pool_ids).bind(&input.signing_profiles).bind(input.model_profile_id)
            .bind(&input.build_context).bind(serde_json::to_value(&input.source_credentials).map_err(anyhow::Error::from)?)
            .bind(input.planner_backend.as_deref().unwrap_or("poll")).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn update_project(&self, id: Uuid, input: &ProjectInput) -> Result<()> {
        sqlx::query("UPDATE projects SET name=$2,repository_url=$3,credential_id=$4,pool_ids=$5,signing_profiles=$6,model_profile_id=$7,build_context=$8,source_credentials=$9,planner_backend=coalesce($10,planner_backend) WHERE id=$1")
            .bind(id).bind(&input.name).bind(&input.repository_url).bind(input.credential_id)
            .bind(&input.pool_ids).bind(&input.signing_profiles).bind(input.model_profile_id)
            .bind(&input.build_context).bind(serde_json::to_value(&input.source_credentials).map_err(anyhow::Error::from)?)
            .bind(&input.planner_backend).execute(&self.pool).await?;
        Ok(())
    }
}
