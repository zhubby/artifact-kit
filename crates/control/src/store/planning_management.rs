//! Planning administration and atomic profile/clarification operations.
use super::ControlStore;
use super::dto;
use super::models::ModelProfile;
use crate::{
    auth::Auth,
    crypto,
    error::{ApiError, Result},
};
use artifactory_database::{params, sql};
use serde_json::{Value, json};
use toasty::stmt::Type;

use uuid::Uuid;

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileInput {
    pub(crate) name: String,
    pub(crate) api: String,
    pub(crate) base_url: String,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) api_key: String,
    #[serde(default = "object")]
    pub(crate) parameters: Value,
    #[serde(default = "yes")]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) is_default: bool,
    #[serde(default = "calls")]
    pub(crate) max_calls: i32,
    #[serde(default = "timeout")]
    pub(crate) timeout_seconds: i32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerUpdate {
    pub(crate) draining: bool,
    pub(crate) revoked: bool,
}

fn object() -> Value {
    json!({})
}

fn yes() -> bool {
    true
}

fn calls() -> i32 {
    20
}

fn timeout() -> i32 {
    600
}

impl ControlStore {
    pub async fn model_profiles(&self) -> Result<Vec<dto::ModelProfile>> {
        Ok(sql::scalars(&mut self.pool.clone(), "SELECT (to_jsonb(m)-'key_ciphertext') || jsonb_build_object('has_key',key_ciphertext<>'','api_key','********') FROM model_profiles m ORDER BY created_at", params![]).await?)
    }

    pub async fn model_options(&self) -> Result<Vec<dto::ModelOption>> {
        Ok(sql::scalars(&mut self.pool.clone(), "SELECT jsonb_build_object('id',id,'name',name,'enabled',enabled) FROM model_profiles WHERE enabled ORDER BY name", params![]).await?)
    }

    pub async fn save_model_profile(
        &self,
        master_key: &[u8; 32],
        id: Uuid,
        input: ProfileInput,
        update: bool,
    ) -> Result<()> {
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        sql::execute(
            &mut tx,
            "SELECT pg_advisory_xact_lock(724319911)",
            params![],
        )
        .await?;
        if input.is_default {
            ModelProfile::filter(ModelProfile::fields().is_default().eq(true))
                .update()
                .is_default(false)
                .exec(&mut tx)
                .await?;
        }
        let mut existing = if update {
            Some(ModelProfile::get_by_id(&mut tx, id).await?)
        } else {
            None
        };
        let ciphertext = if input.api_key.is_empty() || input.api_key == "********" {
            if !update {
                return Err(ApiError::BadRequest("model API key is required".into()));
            }
            existing
                .as_ref()
                .ok_or(ApiError::NotFound)?
                .key_ciphertext
                .clone()
        } else {
            crypto::seal(master_key, input.api_key.as_bytes())?
        };
        if let Some(profile) = existing.as_mut() {
            profile
                .update()
                .name(input.name)
                .api(input.api)
                .base_url(input.base_url.trim_end_matches('/'))
                .model(input.model)
                .key_ciphertext(ciphertext)
                .parameters(toasty::Json(input.parameters))
                .enabled(input.enabled)
                .is_default(input.is_default)
                .max_calls(input.max_calls)
                .timeout_seconds(input.timeout_seconds)
                .revision(toasty::stmt::increment())
                .exec(&mut tx)
                .await?;
        } else {
            ModelProfile::create()
                .id(id)
                .name(input.name)
                .api(input.api)
                .base_url(input.base_url.trim_end_matches('/'))
                .model(input.model)
                .key_ciphertext(ciphertext)
                .parameters(toasty::Json(input.parameters))
                .enabled(input.enabled)
                .is_default(input.is_default)
                .max_calls(input.max_calls)
                .timeout_seconds(input.timeout_seconds)
                .exec(&mut tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_model_profile(&self, id: Uuid) -> Result<()> {
        super::models::ModelProfile::filter(super::models::ModelProfile::fields().id().eq(id))
            .delete()
            .exec(&mut self.pool.clone())
            .await?;
        Ok(())
    }

    pub async fn model_snapshot(&self, id: Option<Uuid>) -> anyhow::Result<Option<Value>> {
        Ok(sql::optional_scalar(&mut self.pool.clone(), "SELECT to_jsonb(m) FROM model_profiles m WHERE enabled AND (($1::uuid IS NOT NULL AND id=$1) OR ($1::uuid IS NULL AND is_default)) LIMIT 1", params![id]).await?)
    }

    pub async fn workers(&self) -> Result<Vec<dto::WorkerSummary>> {
        Ok(sql::scalars(&mut self.pool.clone(), "SELECT (to_jsonb(w)-'certificate_hash') || jsonb_build_object('online',last_seen>now()-interval '30 seconds','active_run',(SELECT id FROM planning_runs r WHERE r.worker_id=w.id AND r.state='leased' LIMIT 1)) FROM ai_workers w ORDER BY created_at", params![]).await?)
    }

    pub async fn update_worker(&self, id: Uuid, input: &WorkerUpdate) -> Result<bool> {
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        let revoked: bool = sql::optional_scalar(
            &mut tx,
            "UPDATE ai_workers SET draining=$2,revoked=revoked OR $3 WHERE id=$1 RETURNING revoked",
            params![id, input.draining, input.revoked],
        )
        .await?
        .ok_or(ApiError::NotFound)?;
        tx.commit().await?;
        Ok(revoked)
    }

    pub async fn planner_enrollment(&self, id: Uuid, hash: String) -> Result<()> {
        sql::execute(&mut self.pool.clone(), "INSERT INTO ai_enrollment_tokens(id,token_hash,expires_at) VALUES($1,$2,now()+interval '15 minutes')", params![id, hash]).await?;
        Ok(())
    }

    pub async fn source_metrics(&self) -> Result<Vec<dto::SourceRepositorySummary>> {
        Ok(sql::scalars(&mut self.pool.clone(), "SELECT to_jsonb(s)-'cache_key' FROM source_repositories s ORDER BY last_used_at DESC LIMIT 100", params![]).await?)
    }

    pub async fn clarify_build(&self, id: Uuid, answer: &str, auth: &Auth) -> Result<()> {
        let mut tx_db = self.pool.clone();
        let mut tx = tx_db.transaction().await?;
        let row=sql::record(&mut tx, "SELECT project_id,status,cancel_requested,planning_context,clarification FROM builds WHERE id=$1 FOR UPDATE", params![id], &[("project_id", Type::Uuid),("status", Type::String),("cancel_requested", Type::Bool),("planning_context", Type::String),("clarification", Type::String)]).await?;
        auth.project(row.decode("project_id")?, "build")?;
        if row.decode::<String>("status")? != "needs_input"
            || row.decode::<bool>("cancel_requested")?
        {
            return Err(ApiError::Conflict("build is not waiting for input".into()));
        }
        let previous=sql::record(&mut tx, "SELECT model_config,failed_targets FROM planning_runs WHERE build_id=$1 ORDER BY created_at DESC LIMIT 1", params![id], &[("model_config", Type::String),("failed_targets", Type::List(Box::new(Type::String)))]).await?;
        let mut context: Value = row.decode("planning_context")?;
        if !context["clarifications"].is_array() {
            context["clarifications"] = json!([]);
        }
        context["clarifications"].as_array_mut().unwrap().push(
            json!({"question":row.decode::<Option<Value>>("clarification")?,"answer":answer}),
        );
        super::models::PlanningRun::create()
            .id(Uuid::new_v4())
            .build_id(id)
            .purpose("clarification")
            .model_config(toasty::Json(
                (previous.decode::<Value>("model_config")?).clone(),
            ))
            .failed_targets(previous.decode::<Vec<String>>("failed_targets")?)
            .exec(&mut tx)
            .await?;
        sql::execute(&mut tx, "UPDATE builds SET planning_context=$2,clarification=NULL,status=CASE WHEN current_plan_id IS NULL THEN 'planning' ELSE 'replanning' END WHERE id=$1", params![id, context]).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn plans(&self, id: Uuid, auth: &Auth) -> Result<dto::BuildPlans> {
        let project: Uuid = sql::scalar(
            &mut self.pool.clone(),
            "SELECT project_id FROM builds WHERE id=$1",
            params![id],
        )
        .await?;
        auth.project(project, "read")?;
        let plans: Vec<dto::PlanRevision> = sql::scalars(
            &mut self.pool.clone(),
            "SELECT to_jsonb(p) FROM plan_revisions p WHERE build_id=$1 ORDER BY revision",
            params![id],
        )
        .await?;
        let runs:Vec<dto::PlanningRun>=sql::scalars(&mut self.pool.clone(), "SELECT to_jsonb(r)-'model_config'-'lease_hash' FROM planning_runs r WHERE build_id=$1 ORDER BY created_at", params![id]).await?;
        let events:Vec<dto::PlanningEvent>=sql::scalars(&mut self.pool.clone(), "SELECT to_jsonb(e) FROM planning_events e JOIN planning_runs r ON r.id=e.run_id WHERE r.build_id=$1 AND e.kind<>'checkpoint' ORDER BY e.id DESC LIMIT 300", params![id]).await?;
        Ok(dto::BuildPlans {
            plans,
            runs,
            events,
        })
    }
}
