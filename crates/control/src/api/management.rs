use crate::{
    auth::{self, Auth},
    crypto,
    error::{ApiError, Result},
    source::GitCredential,
    state::AppState,
};
use axum::{
    Json,
    extract::{Path, State},
};
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

pub async fn overview(State(state): State<AppState>, auth: Auth) -> Result<Json<Value>> {
    auth.session_role("viewer")?;
    let value: Value = sqlx::query_scalar("SELECT jsonb_build_object('projects',(SELECT count(*) FROM projects),'nodes_online',(SELECT count(*) FROM nodes WHERE NOT revoked AND last_seen>now()-interval '30 seconds'),'running',(SELECT count(*) FROM jobs WHERE status IN ('leased','running')),'queued',(SELECT count(*) FROM jobs WHERE status='queued'),'releases',(SELECT count(*) FROM releases),'builds_today',(SELECT count(*) FROM builds WHERE created_at>now()-interval '24 hours'))")
        .fetch_one(&state.pool).await?;
    Ok(Json(value))
}

pub async fn users(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Value>>> {
    auth.session_role("admin")?;
    Ok(Json(sqlx::query_scalar("SELECT jsonb_build_object('id',id,'username',username,'role',role,'created_at',created_at) FROM users ORDER BY created_at").fetch_all(&state.pool).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewUser {
    username: String,
    password: String,
    role: String,
}

pub async fn create_user(
    State(state): State<AppState>,
    auth: Auth,
    Json(input): Json<NewUser>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    if input.username.is_empty()
        || input.username.len() > 100
        || !["admin", "operator", "viewer"].contains(&input.role.as_str())
    {
        return Err(ApiError::BadRequest("invalid user".into()));
    }
    let password = auth::password_hash(&input.password)
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,password_hash,role) VALUES($1,$2,$3,$4)")
        .bind(id)
        .bind(input.username)
        .bind(password)
        .bind(input.role)
        .execute(&state.pool)
        .await?;
    state
        .audit(auth.user_id, "user_created", id, json!({}))
        .await?;
    Ok(Json(json!({"id":id})))
}

pub async fn tokens(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Value>>> {
    auth.session_role("viewer")?;
    Ok(Json(sqlx::query_scalar("SELECT to_jsonb(t)-'token_hash' FROM api_tokens t WHERE user_id=$1 ORDER BY created_at DESC").bind(auth.user_id).fetch_all(&state.pool).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewToken {
    name: String,
    project_ids: Vec<Uuid>,
    scopes: Vec<String>,
    days: i64,
}

pub async fn create_token(
    State(state): State<AppState>,
    auth: Auth,
    Json(input): Json<NewToken>,
) -> Result<Json<Value>> {
    auth.session_role("viewer")?;
    if input.name.is_empty()
        || input.name.len() > 100
        || input.project_ids.is_empty()
        || input.scopes.is_empty()
        || !(1..=365).contains(&input.days)
        || input
            .scopes
            .iter()
            .any(|scope| !["read", "build", "release"].contains(&scope.as_str()))
    {
        return Err(ApiError::BadRequest("invalid token scope or expiry".into()));
    }
    for project in &input.project_ids {
        for scope in &input.scopes {
            auth.project(*project, scope)?;
        }
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM projects WHERE id=$1)")
            .bind(project)
            .fetch_one(&state.pool)
            .await?;
        if !exists {
            return Err(ApiError::NotFound);
        }
    }
    let id = Uuid::new_v4();
    let token = format!("artifactory_{}", crypto::token());
    sqlx::query("INSERT INTO api_tokens(id,user_id,name,token_hash,project_ids,scopes,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(id).bind(auth.user_id).bind(input.name).bind(crypto::hash(&token)).bind(input.project_ids).bind(input.scopes).bind(Utc::now()+Duration::days(input.days)).execute(&state.pool).await?;
    state
        .audit(auth.user_id, "api_token_created", id, json!({}))
        .await?;
    Ok(Json(json!({"id":id,"token":token})))
}

pub async fn revoke_token(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    auth.session_role("viewer")?;
    sqlx::query("UPDATE api_tokens SET revoked=true WHERE id=$1 AND user_id=$2")
        .bind(id)
        .bind(auth.user_id)
        .execute(&state.pool)
        .await?;
    state
        .audit(auth.user_id, "api_token_revoked", id, json!({}))
        .await?;
    Ok(Json(json!({"ok":true})))
}

pub async fn pools(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Value>>> {
    auth.session_role("viewer")?;
    Ok(Json(
        sqlx::query_scalar("SELECT to_jsonb(p) FROM node_pools p ORDER BY name")
            .fetch_all(&state.pool)
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Name {
    name: String,
}

pub async fn create_pool(
    State(state): State<AppState>,
    auth: Auth,
    Json(input): Json<Name>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    if input.name.is_empty() || input.name.len() > 100 {
        return Err(ApiError::BadRequest("invalid pool name".into()));
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO node_pools(id,name) VALUES($1,$2)")
        .bind(id)
        .bind(input.name)
        .execute(&state.pool)
        .await?;
    state
        .audit(auth.user_id, "pool_created", id, json!({}))
        .await?;
    Ok(Json(json!({"id":id})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewEnrollment {
    pool_id: Uuid,
}

pub async fn enrollment_token(
    State(state): State<AppState>,
    auth: Auth,
    Json(input): Json<NewEnrollment>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    let id = Uuid::new_v4();
    let token = crypto::token();
    sqlx::query("INSERT INTO enrollment_tokens(id,token_hash,pool_id,expires_at) VALUES($1,$2,$3,now()+interval '15 minutes')").bind(id).bind(crypto::hash(&token)).bind(input.pool_id).execute(&state.pool).await?;
    state
        .audit(
            auth.user_id,
            "enrollment_token_created",
            id,
            json!({"pool_id":input.pool_id}),
        )
        .await?;
    Ok(Json(json!({"id":id,"token":token,"expires_seconds":900})))
}

pub async fn nodes(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Value>>> {
    auth.session_role("viewer")?;
    Ok(Json(sqlx::query_scalar("SELECT (to_jsonb(n)-'certificate_hash'-'active_session') || jsonb_build_object('emissary_desired_release',coalesce(n.emissary_target_release,p.emissary_target_release),'emissary_pool_paused',p.emissary_updates_paused,'pool_name',p.name,'online',NOT n.revoked AND n.last_seen>now()-interval '30 seconds','active_jobs',(SELECT count(*) FROM attempts a WHERE a.node_id=n.id AND a.status IN ('leased','running') AND a.lease_expires_at>now())) FROM nodes n JOIN node_pools p ON p.id=n.pool_id ORDER BY n.created_at DESC").fetch_all(&state.pool).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeUpdate {
    draining: bool,
    revoked: bool,
    slots: i32,
    labels: Value,
    signing_profiles: Vec<String>,
    pool_id: Uuid,
}

pub async fn update_node(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(input): Json<NodeUpdate>,
) -> Result<Json<Value>> {
    auth.session_role("admin")?;
    if !(1..=32).contains(&input.slots)
        || !input.labels.is_object()
        || input.labels.to_string().len() > 8192
        || input.signing_profiles.len() > 32
    {
        return Err(ApiError::BadRequest("invalid node settings".into()));
    }
    let mut transaction = state.pool.begin().await?;
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    let changing_pool_while_updating: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND ota_draining AND pool_id<>$2)",
    )
    .bind(id)
    .bind(input.pool_id)
    .fetch_one(&mut *transaction)
    .await?;
    if changing_pool_while_updating {
        return Err(ApiError::Conflict(
            "wait for the active Emissary update before changing its pool".into(),
        ));
    }
    let revoked=sqlx::query_scalar::<_,bool>("UPDATE nodes SET draining=$2,revoked=revoked OR $3,slots=$4,labels=$5,signing_profiles=$6,pool_id=$7 WHERE id=$1 RETURNING revoked").bind(id).bind(input.draining).bind(input.revoked).bind(input.slots).bind(input.labels).bind(input.signing_profiles).bind(input.pool_id).fetch_optional(&mut *transaction).await?.ok_or(ApiError::NotFound)?;
    transaction.commit().await?;
    state
        .audit(
            auth.user_id,
            "node_updated",
            id,
            json!({"draining":input.draining,"revoked":revoked}),
        )
        .await?;
    Ok(Json(json!({"ok":true})))
}

pub async fn credentials(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Value>>> {
    auth.session_role("admin")?;
    Ok(Json(
        sqlx::query_scalar(
            "SELECT to_jsonb(c)-'ciphertext' FROM credentials c ORDER BY created_at DESC",
        )
        .fetch_all(&state.pool)
        .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewCredential {
    name: String,
    kind: String,
    #[serde(default)]
    username: String,
    secret: String,
    #[serde(default)]
    known_hosts: String,
}

pub async fn create_credential(
    State(state): State<AppState>,
    actor: Auth,
    Json(input): Json<NewCredential>,
) -> Result<Json<Value>> {
    actor.session_role("admin")?;
    let input_name = input.name;
    let input = GitCredential {
        kind: input.kind,
        username: input.username,
        secret: input.secret,
        known_hosts: input.known_hosts,
    };
    if input_name.is_empty()
        || input_name.len() > 100
        || !["ssh", "https"].contains(&input.kind.as_str())
        || input.secret.is_empty()
        || input.secret.len() > 65536
        || (input.kind == "ssh" && input.known_hosts.trim().is_empty())
    {
        return Err(ApiError::BadRequest(
            "invalid credential; SSH requires known_hosts".into(),
        ));
    }
    let id = Uuid::new_v4();
    let encrypted = crypto::seal(
        &state.settings.master_key,
        &serde_json::to_vec(&input).map_err(anyhow::Error::from)?,
    )?;
    sqlx::query("INSERT INTO credentials(id,name,kind,ciphertext) VALUES($1,$2,$3,$4)")
        .bind(id)
        .bind(input_name)
        .bind(input.kind)
        .bind(encrypted)
        .execute(&state.pool)
        .await?;
    state
        .audit(actor.user_id, "credential_created", id, json!({}))
        .await?;
    Ok(Json(json!({"id":id})))
}

pub async fn audit(State(state): State<AppState>, auth: Auth) -> Result<Json<Vec<Value>>> {
    auth.session_role("admin")?;
    Ok(Json(
        sqlx::query_scalar("SELECT to_jsonb(a) FROM audit_events a ORDER BY id DESC LIMIT 200")
            .fetch_all(&state.pool)
            .await?,
    ))
}
