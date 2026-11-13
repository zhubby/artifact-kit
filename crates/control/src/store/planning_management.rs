//! Planning administration and atomic profile/clarification operations.
use super::ControlStore;
use crate::{
    auth::Auth,
    crypto,
    error::{ApiError, Result},
};
use serde_json::{Value, json};
use sqlx::Row;
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
    pub async fn model_profiles(&self) -> Result<Vec<Value>> {
        Ok(sqlx::query_scalar("SELECT (to_jsonb(m)-'key_ciphertext') || jsonb_build_object('has_key',key_ciphertext<>'','api_key','********') FROM model_profiles m ORDER BY created_at")
        .fetch_all(&self.pool).await?)
    }

    pub async fn model_options(&self) -> Result<Vec<Value>> {
        Ok(sqlx::query_scalar("SELECT jsonb_build_object('id',id,'name',name,'enabled',enabled) FROM model_profiles WHERE enabled ORDER BY name").fetch_all(&self.pool).await?)
    }

    pub async fn save_model_profile(
        &self,
        master_key: &[u8; 32],
        id: Uuid,
        input: ProfileInput,
        update: bool,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(724319911)")
            .execute(&mut *tx)
            .await?;
        if input.is_default {
            sqlx::query("UPDATE model_profiles SET is_default=false WHERE is_default")
                .execute(&mut *tx)
                .await?;
        }
        let ciphertext = if input.api_key.is_empty() || input.api_key == "********" {
            if !update {
                return Err(ApiError::BadRequest("model API key is required".into()));
            }
            sqlx::query_scalar::<_, String>("SELECT key_ciphertext FROM model_profiles WHERE id=$1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?
        } else {
            crypto::seal(master_key, input.api_key.as_bytes())?
        };
        let affected=if update {
        sqlx::query("UPDATE model_profiles SET name=$2,api=$3,base_url=$4,model=$5,key_ciphertext=$6,parameters=$7,enabled=$8,is_default=$9,max_calls=$10,timeout_seconds=$11,revision=revision+1 WHERE id=$1")
    } else {
        sqlx::query("INSERT INTO model_profiles(id,name,api,base_url,model,key_ciphertext,parameters,enabled,is_default,max_calls,timeout_seconds) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
    }.bind(id).bind(input.name).bind(input.api).bind(input.base_url.trim_end_matches('/')).bind(input.model).bind(ciphertext).bind(input.parameters).bind(input.enabled).bind(input.is_default).bind(input.max_calls).bind(input.timeout_seconds)
        .execute(&mut *tx).await?;
        if affected.rows_affected() != 1 {
            return Err(ApiError::NotFound);
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_model_profile(&self, id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM model_profiles WHERE id=$1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn model_snapshot(&self, id: Option<Uuid>) -> anyhow::Result<Option<Value>> {
        Ok(sqlx::query_scalar("SELECT to_jsonb(m) FROM model_profiles m WHERE enabled AND (($1::uuid IS NOT NULL AND id=$1) OR ($1::uuid IS NULL AND is_default)) LIMIT 1")
        .bind(id).fetch_optional(&self.pool).await?)
    }

    pub async fn workers(&self) -> Result<Vec<Value>> {
        Ok(sqlx::query_scalar("SELECT (to_jsonb(w)-'certificate_hash') || jsonb_build_object('online',last_seen>now()-interval '30 seconds','active_run',(SELECT id FROM planning_runs r WHERE r.worker_id=w.id AND r.state='leased' LIMIT 1)) FROM ai_workers w ORDER BY created_at")
        .fetch_all(&self.pool).await?)
    }

    pub async fn update_worker(&self, id: Uuid, input: &WorkerUpdate) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let revoked: bool = sqlx::query_scalar(
            "UPDATE ai_workers SET draining=$2,revoked=revoked OR $3 WHERE id=$1 RETURNING revoked",
        )
        .bind(id)
        .bind(input.draining)
        .bind(input.revoked)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ApiError::NotFound)?;
        tx.commit().await?;
        Ok(revoked)
    }

    pub async fn planner_enrollment(&self, id: Uuid, hash: String) -> Result<()> {
        sqlx::query("INSERT INTO ai_enrollment_tokens(id,token_hash,expires_at) VALUES($1,$2,now()+interval '15 minutes')").bind(id).bind(hash).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn source_metrics(&self) -> Result<Vec<Value>> {
        Ok(sqlx::query_scalar("SELECT to_jsonb(s)-'cache_key' FROM source_repositories s ORDER BY last_used_at DESC LIMIT 100").fetch_all(&self.pool).await?)
    }

    pub async fn clarify_build(&self, id: Uuid, answer: &str, auth: &Auth) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT project_id,status,cancel_requested,planning_context,clarification FROM builds WHERE id=$1 FOR UPDATE").bind(id).fetch_one(&mut *tx).await?;
        auth.project(row.get("project_id"), "build")?;
        if row.get::<String, _>("status") != "needs_input" || row.get::<bool, _>("cancel_requested")
        {
            return Err(ApiError::Conflict("build is not waiting for input".into()));
        }
        let previous=sqlx::query("SELECT model_config,failed_targets FROM planning_runs WHERE build_id=$1 ORDER BY created_at DESC LIMIT 1").bind(id).fetch_one(&mut *tx).await?;
        let mut context: Value = row.get("planning_context");
        if !context["clarifications"].is_array() {
            context["clarifications"] = json!([]);
        }
        context["clarifications"]
            .as_array_mut()
            .unwrap()
            .push(json!({"question":row.get::<Option<Value>,_>("clarification"),"answer":answer}));
        sqlx::query("INSERT INTO planning_runs(id,build_id,purpose,model_config,failed_targets) VALUES($1,$2,'clarification',$3,$4)").bind(Uuid::new_v4()).bind(id).bind(previous.get::<Value,_>("model_config")).bind(previous.get::<Vec<String>,_>("failed_targets")).execute(&mut *tx).await?;
        sqlx::query("UPDATE builds SET planning_context=$2,clarification=NULL,status=CASE WHEN current_plan_id IS NULL THEN 'planning' ELSE 'replanning' END WHERE id=$1").bind(id).bind(context).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn plans(&self, id: Uuid, auth: &Auth) -> Result<Value> {
        let project: Uuid = sqlx::query_scalar("SELECT project_id FROM builds WHERE id=$1")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        auth.project(project, "read")?;
        let plans: Vec<Value> = sqlx::query_scalar(
            "SELECT to_jsonb(p) FROM plan_revisions p WHERE build_id=$1 ORDER BY revision",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        let runs:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(r)-'model_config'-'lease_hash' FROM planning_runs r WHERE build_id=$1 ORDER BY created_at").bind(id).fetch_all(&self.pool).await?;
        let events:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(e) FROM planning_events e JOIN planning_runs r ON r.id=e.run_id WHERE r.build_id=$1 AND e.kind<>'checkpoint' ORDER BY e.id DESC LIMIT 300").bind(id).fetch_all(&self.pool).await?;
        Ok(json!({"plans":plans,"runs":runs,"events":events}))
    }
}
