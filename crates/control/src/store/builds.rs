//! Build, artifact and release persistence. External storage IO stays in services.
use super::ControlStore;
use super::dto;
use crate::{
    auth::Auth,
    error::{ApiError, Result},
};
use artifactory_database::{params, sql};
use serde::Deserialize;
use serde_json::{Value, json};
use toasty::stmt::Type;

use uuid::Uuid;

pub struct ArtifactDownload {
    pub object_key: String,
    pub project_id: Uuid,
}
pub struct LogChunk {
    pub id: i64,
    pub attempt_id: Uuid,
    pub step: String,
    pub object_key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildInput {
    #[serde(rename = "ref")]
    pub(crate) reference: String,
    #[serde(default = "empty_object")]
    pub(crate) parameters: Value,
    #[serde(default)]
    pub(crate) targets: Vec<String>,
    pub(crate) release_version: Option<String>,
    pub(crate) goal: Option<String>,
    #[serde(default = "empty_array")]
    pub(crate) target_requirements: Value,
}
fn empty_array() -> Value {
    json!([])
}
fn empty_object() -> Value {
    json!({})
}

impl ControlStore {
    pub async fn create_build(
        &self,
        project: Uuid,
        actor: Uuid,
        input: &BuildInput,
        goal: String,
    ) -> Result<Uuid> {
        let project_row=sql::record(&mut self.pool.clone(), "SELECT model_profile_id,pool_ids,signing_profiles,build_context,source_credentials FROM projects WHERE id=$1", params![project], &[("model_profile_id", Type::Uuid),("pool_ids", Type::List(Box::new(Type::Uuid))),("signing_profiles", Type::List(Box::new(Type::String))),("build_context", Type::String),("source_credentials", Type::String)]).await?;
        let model = self
            .model_snapshot(project_row.decode("model_profile_id")?)
            .await?;
        let context = json!({"pool_ids":project_row.decode::<Vec<Uuid>>("pool_ids")?,"signing_profiles":project_row.decode::<Vec<String>>("signing_profiles")?,"build_context":project_row.decode::<String>("build_context")?,"source_credentials":project_row.decode::<Value>("source_credentials")?});
        let id = Uuid::new_v4();
        let created=sql::execute(&mut self.pool.clone(), "INSERT INTO builds(id,project_id,requested_ref,parameters,selected_targets,release_version,publish_status,requested_by,source_repository_url,source_credential_id,goal,target_requirements,planning_context,model_config_snapshot) SELECT $1,id,$3,$4,$5,$6,$7,$8,repository_url,credential_id,$9,$10,$11,$12 FROM projects WHERE id=$2", params![id, project, &input.reference, &input.parameters, &input.targets, &input.release_version, if input.release_version.is_some(){"pending"}else{"not_requested"}, actor, goal, &input.target_requirements, context, model]).await?;
        if created != 1 {
            return Err(ApiError::NotFound);
        }
        Ok(id)
    }

    pub async fn builds(
        &self,
        project: Option<Uuid>,
        visible: Option<Vec<Uuid>>,
    ) -> Result<Vec<dto::BuildSummary>> {
        Ok(sql::scalars(&mut self.pool.clone(), "SELECT (to_jsonb(b)-'config_yaml'-'config_snapshot'-'model_config_snapshot') || jsonb_build_object('project_name',p.name,'targets',(SELECT coalesce(jsonb_agg(jsonb_build_object('id',j.id,'target',j.target,'status',j.status)),'[]') FROM jobs j WHERE j.build_id=b.id)) FROM builds b JOIN projects p ON p.id=b.project_id WHERE ($1::uuid IS NULL OR b.project_id=$1) AND ($2::uuid[] IS NULL OR b.project_id=ANY($2)) ORDER BY b.created_at DESC LIMIT 100", params![project, visible]).await?)
    }

    pub async fn artifacts(
        &self,
        project: Option<Uuid>,
        visible: Option<Vec<Uuid>>,
    ) -> Result<Vec<dto::ArtifactSummary>> {
        Ok(sql::scalars(&mut self.pool.clone(), "SELECT (to_jsonb(ar)-'object_key') || jsonb_build_object('target',j.target,'build_id',b.id,'project_name',p.name,'project_id',p.id) FROM artifacts ar JOIN attempts a ON a.id=ar.attempt_id JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id JOIN projects p ON p.id=b.project_id WHERE ($1::uuid IS NULL OR p.id=$1) AND ($2::uuid[] IS NULL OR p.id=ANY($2)) ORDER BY ar.created_at DESC LIMIT 100", params![project, visible]).await?)
    }

    pub async fn releases(
        &self,
        project: Option<Uuid>,
        visible: Option<Vec<Uuid>>,
    ) -> Result<Vec<dto::ReleaseSummary>> {
        Ok(sql::scalars(&mut self.pool.clone(), "SELECT to_jsonb(r) || jsonb_build_object('project_name',p.name,'artifact_count',(SELECT count(*) FROM release_artifacts ra WHERE ra.release_id=r.id)) FROM releases r JOIN projects p ON p.id=r.project_id WHERE ($1::uuid IS NULL OR p.id=$1) AND ($2::uuid[] IS NULL OR p.id=ANY($2)) ORDER BY r.created_at DESC LIMIT 100", params![project, visible]).await?)
    }

    pub async fn build_project(&self, id: Uuid) -> Result<Uuid> {
        Ok(sql::scalar(
            &mut self.pool.clone(),
            "SELECT project_id FROM builds WHERE id=$1",
            params![id],
        )
        .await?)
    }

    pub async fn build_detail(&self, id: Uuid) -> Result<dto::BuildDetail> {
        let build:dto::BuildEnvelope=sql::scalar(&mut self.pool.clone(), "SELECT (to_jsonb(b)-'model_config_snapshot') || jsonb_build_object('project_name',p.name) FROM builds b JOIN projects p ON p.id=b.project_id WHERE b.id=$1", params![id]).await?;
        let jobs:Vec<dto::Job>=sql::scalars(&mut self.pool.clone(), "SELECT to_jsonb(j) || jsonb_build_object('attempts',(SELECT coalesce(jsonb_agg((to_jsonb(a)-'lease_token'-'command_id') || jsonb_build_object('node_name',n.name,'steps',(SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY s.step_index),'[]') FROM step_runs s WHERE s.attempt_id=a.id)) ORDER BY a.created_at),'[]') FROM attempts a JOIN nodes n ON n.id=a.node_id WHERE a.job_id=j.id)) FROM jobs j WHERE j.build_id=$1 ORDER BY j.target", params![id]).await?;
        let artifacts:Vec<dto::BuildArtifact>=sql::scalars(&mut self.pool.clone(), "SELECT (to_jsonb(ar)-'object_key') || jsonb_build_object('target',j.target,'attempt_status',a.status) FROM artifacts ar JOIN attempts a ON a.id=ar.attempt_id JOIN jobs j ON j.id=a.job_id WHERE j.build_id=$1 ORDER BY ar.created_at", params![id]).await?;
        let publish_attempts = sql::scalars::<dto::PublishAttempt>(
            &mut self.pool.clone(),
            "SELECT to_jsonb(p) FROM publish_attempts p WHERE build_id=$1 ORDER BY created_at DESC",
            params![id],
        )
        .await?;
        Ok(dto::BuildDetail {
            build,
            jobs,
            artifacts,
            publish_attempts,
        })
    }

    pub async fn cancel_build(&self, id: Uuid) -> Result<()> {
        let mut transaction_db = self.pool.clone();
        let mut transaction = transaction_db.transaction().await?;
        let status: String = sql::scalar(
            &mut transaction,
            "SELECT status FROM builds WHERE id=$1 FOR UPDATE",
            params![id],
        )
        .await?;
        if ["succeeded", "failed", "lost", "canceled"].contains(&status.as_str()) {
            return Err(ApiError::Conflict("build is already terminal".into()));
        }
        sql::execute(
            &mut transaction,
            "UPDATE builds SET cancel_requested=true,status='canceling' WHERE id=$1",
            params![id],
        )
        .await?;
        sql::execute(
            &mut transaction,
            "UPDATE jobs SET status='canceled' WHERE build_id=$1 AND status='queued'",
            params![id],
        )
        .await?;
        sql::execute(&mut transaction, "UPDATE planning_runs SET state='canceled',finished_at=now() WHERE build_id=$1 AND state IN ('queued','leased')", params![id]).await?;
        if ["resolving", "planning", "replanning", "needs_input"].contains(&status.as_str()) {
            sql::execute(
                &mut transaction,
                "UPDATE builds SET status='canceled',finished_at=now() WHERE id=$1",
                params![id],
            )
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn retry_job(&self, id: Uuid, auth: &Auth) -> Result<()> {
        let mut transaction_db = self.pool.clone();
        let mut transaction = transaction_db.transaction().await?;
        let row=sql::record(&mut transaction, "SELECT j.status,b.status AS build_status,b.id,b.project_id,b.release_version,EXISTS(SELECT 1 FROM releases r WHERE r.build_id=b.id) AS published FROM jobs j JOIN builds b ON b.id=j.build_id WHERE j.id=$1 FOR UPDATE OF j,b", params![id], &[("status", Type::String),("build_status", Type::String),("id", Type::Uuid),("project_id", Type::Uuid),("release_version", Type::String),("published", Type::Bool)]).await?;
        auth.project(row.decode("project_id")?, "build")?;
        if row.decode::<Option<String>>("release_version")?.is_some() {
            auth.project(row.decode("project_id")?, "release")?;
        }
        if row.decode::<bool>("published")?
            || ["planning", "replanning", "needs_input"]
                .contains(&row.decode::<String>("build_status")?.as_str())
            || !["failed", "lost", "canceled"].contains(&row.decode::<String>("status")?.as_str())
        {
            return Err(ApiError::Conflict(
                "only failed, lost or canceled unpublished jobs can be retried".into(),
            ));
        }
        sql::execute(
            &mut transaction,
            "UPDATE jobs SET status='queued' WHERE id=$1",
            params![id],
        )
        .await?;
        sql::execute(&mut transaction, "UPDATE builds SET status='queued',cancel_requested=false,finished_at=NULL,publish_status=CASE WHEN release_version IS NULL THEN 'not_requested' ELSE 'pending' END,publish_error=NULL WHERE id=$1", params![row.decode::<Uuid>("id")?]).await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn request_publication(&self, id: Uuid, version: Option<String>) -> Result<()> {
        let mut transaction_db = self.pool.clone();
        let mut transaction = transaction_db.transaction().await?;
        let row = sql::record(
            &mut transaction,
            "SELECT status,release_version FROM builds WHERE id=$1 FOR UPDATE",
            params![id],
            &[("status", Type::String), ("release_version", Type::String)],
        )
        .await?;
        if row.decode::<String>("status")? != "succeeded" {
            return Err(ApiError::Conflict(
                "build must succeed before publication".into(),
            ));
        }
        let current: Option<String> = row.decode("release_version")?;
        if let Some(version) = version {
            crate::build::validate_release_version(&version)?;
            if current.as_ref().is_some_and(|current| current != &version) {
                return Err(ApiError::Conflict(
                    "release version is immutable for this build".into(),
                ));
            }
            sql::execute(
                &mut transaction,
                "UPDATE builds SET release_version=$2 WHERE id=$1",
                params![id, version],
            )
            .await?;
        } else if current.is_none() {
            return Err(ApiError::BadRequest("version is required".into()));
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn artifact_download(&self, id: Uuid) -> Result<ArtifactDownload> {
        let row=sql::record(&mut self.pool.clone(), "SELECT ar.object_key,b.project_id FROM artifacts ar JOIN attempts a ON a.id=ar.attempt_id JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE ar.id=$1", params![id], &[("object_key", Type::String),("project_id", Type::Uuid)]).await?;
        Ok(ArtifactDownload {
            object_key: row.decode("object_key")?,
            project_id: row.decode("project_id")?,
        })
    }

    pub async fn release(&self, id: Uuid) -> Result<dto::Release> {
        let release:dto::Release=sql::scalar(&mut self.pool.clone(), "SELECT to_jsonb(r) || jsonb_build_object('project_name',p.name) FROM releases r JOIN projects p ON p.id=r.project_id WHERE r.id=$1", params![id]).await?;
        Ok(release)
    }

    pub async fn release_artifacts(&self, id: Uuid) -> Result<Vec<dto::ReleaseArtifact>> {
        Ok(sql::scalars::<dto::ReleaseArtifact>(&mut self.pool.clone(), "SELECT (to_jsonb(ar)-'object_key') || jsonb_build_object('target',j.target) FROM release_artifacts ra JOIN artifacts ar ON ar.id=ra.artifact_id JOIN attempts a ON a.id=ar.attempt_id JOIN jobs j ON j.id=a.job_id WHERE ra.release_id=$1 ORDER BY j.target,ar.path", params![id]).await?)
    }

    pub async fn log_chunks(&self, id: Uuid, cursor: i64) -> Result<Vec<LogChunk>> {
        let rows=sql::records(&mut self.pool.clone(), "SELECT l.id,l.attempt_id,l.step,l.object_key FROM log_chunks l JOIN attempts a ON a.id=l.attempt_id JOIN jobs j ON j.id=a.job_id WHERE j.build_id=$1 AND l.id>$2 ORDER BY l.id LIMIT 100", params![id, cursor], &[("id", Type::I64),("attempt_id", Type::Uuid),("step", Type::String),("object_key", Type::String)]).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                Ok::<_, artifactory_database::Error>(LogChunk {
                    id: row.decode("id")?,
                    attempt_id: row.decode("attempt_id")?,
                    step: row.decode("step")?,
                    object_key: row.decode("object_key")?,
                })
            })
            .collect::<artifactory_database::Result<_>>()?)
    }
}
