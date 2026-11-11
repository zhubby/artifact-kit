use crate::{
    crypto,
    error::{ApiError, Result},
    state::AppState,
};
use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::{SaltString, rand_core::OsRng},
};
use axum::{
    Json,
    extract::{FromRequestParts, State},
    http::{HeaderMap, HeaderValue, Method, header, request::Parts},
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct Auth {
    pub user_id: Uuid,
    pub username: String,
    pub role: String,
    pub token_projects: Option<Vec<Uuid>>,
    pub token_scopes: Vec<String>,
}

impl Auth {
    pub fn scoped(user_id: Uuid, role: &str, projects: Vec<Uuid>, scopes: Vec<String>) -> Self {
        Self {
            user_id,
            username: String::new(),
            role: role.into(),
            token_projects: Some(projects),
            token_scopes: scopes,
        }
    }

    pub fn session_role(&self, role: &str) -> Result<()> {
        if self.token_projects.is_some() || rank(&self.role) < rank(role) {
            return Err(ApiError::Forbidden);
        }
        Ok(())
    }

    pub fn project(&self, project: Uuid, operation: &str) -> Result<()> {
        if rank(&self.role) < if operation == "read" { 0 } else { 1 } {
            return Err(ApiError::Forbidden);
        }
        if let Some(projects) = &self.token_projects
            && (!projects.contains(&project)
                || !self.token_scopes.iter().any(|scope| scope == operation))
        {
            return Err(ApiError::Forbidden);
        }
        Ok(())
    }

    pub fn visible_projects(&self) -> Option<Vec<Uuid>> {
        self.token_projects.clone()
    }
}

fn rank(role: &str) -> i32 {
    match role {
        "admin" => 2,
        "operator" => 1,
        "viewer" => 0,
        _ => -1,
    }
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self> {
        if let Some(value) = parts.headers.get(header::AUTHORIZATION) {
            let value = value.to_str().map_err(|_| ApiError::Unauthorized)?;
            let token = value
                .strip_prefix("Bearer ")
                .ok_or(ApiError::Unauthorized)?;
            let row = sqlx::query("SELECT u.id,u.username,u.role,t.project_ids,t.scopes FROM api_tokens t JOIN users u ON u.id=t.user_id WHERE t.token_hash=$1 AND NOT t.revoked AND t.expires_at>now()")
                .bind(crypto::hash(token)).fetch_optional(&state.pool).await?.ok_or(ApiError::Unauthorized)?;
            return Ok(Self {
                user_id: row.get("id"),
                username: row.get("username"),
                role: row.get("role"),
                token_projects: Some(row.get("project_ids")),
                token_scopes: row.get("scopes"),
            });
        }
        if ![Method::GET, Method::HEAD, Method::OPTIONS].contains(&parts.method) {
            csrf(&parts.headers, state)?;
        }
        let token = cookie(&parts.headers).ok_or(ApiError::Unauthorized)?;
        let row = sqlx::query("SELECT u.id,u.username,u.role FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.token_hash=$1 AND s.expires_at>now()")
            .bind(crypto::hash(token)).fetch_optional(&state.pool).await?.ok_or(ApiError::Unauthorized)?;
        Ok(Self {
            user_id: row.get("id"),
            username: row.get("username"),
            role: row.get("role"),
            token_projects: None,
            token_scopes: vec![],
        })
    }
}

pub fn csrf(headers: &HeaderMap, state: &AppState) -> Result<()> {
    if headers
        .get("x-artifactory-csrf")
        .and_then(|value| value.to_str().ok())
        != Some("1")
    {
        return Err(ApiError::Forbidden);
    }
    if let Some(origin) = headers.get(header::ORIGIN)
        && origin.to_str().ok() != Some(state.settings.public_url.as_str())
    {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

fn cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|entry| entry.trim().strip_prefix("artifactory_session="))
}

pub fn password_hash(password: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        password.len() >= 12 && password.len() <= 256,
        "password must contain 12..256 characters"
    );
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
        .map_err(|_| anyhow::anyhow!("password hashing failed"))?
        .to_string())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Login {
    username: String,
    password: String,
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Login>,
) -> Result<impl IntoResponse> {
    csrf(&headers, &state)?;
    if input.username.len() > 100 || input.password.len() > 256 {
        return Err(ApiError::Unauthorized);
    }
    let recent: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='login_failed' AND resource_id=$1 AND created_at>now()-interval '5 minutes'")
        .bind(&input.username).fetch_one(&state.pool).await?;
    if recent >= 10 {
        return Err(ApiError::Unauthorized);
    }
    let row = sqlx::query("SELECT id,password_hash,role FROM users WHERE username=$1")
        .bind(&input.username)
        .fetch_optional(&state.pool)
        .await?;
    let valid = if let Some(row) = &row {
        let encoded: String = row.get("password_hash");
        let password = input.password.clone();
        tokio::task::spawn_blocking(move || {
            PasswordHash::new(&encoded).is_ok_and(|encoded| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &encoded)
                    .is_ok()
            })
        })
        .await
        .unwrap_or(false)
    } else {
        false
    };
    if !valid {
        state
            .audit("anonymous", "login_failed", &input.username, json!({}))
            .await?;
        return Err(ApiError::Unauthorized);
    }
    let row = row.unwrap();
    let user_id: Uuid = row.get("id");
    let token = crypto::token();
    sqlx::query("INSERT INTO sessions(token_hash,user_id,expires_at) VALUES($1,$2,now()+interval '24 hours')")
        .bind(crypto::hash(&token)).bind(user_id).execute(&state.pool).await?;
    state.audit(user_id, "login", user_id, json!({})).await?;
    let secure = if state.settings.insecure_dev {
        ""
    } else {
        "; Secure"
    };
    let value = HeaderValue::from_str(&format!(
        "artifactory_session={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400{secure}"
    ))
    .unwrap();
    Ok((
        [(header::SET_COOKIE, value)],
        Json(json!({"id":user_id,"username":input.username,"role":row.get::<String,_>("role")})),
    ))
}

pub async fn logout(
    State(state): State<AppState>,
    _auth: Auth,
    headers: HeaderMap,
) -> Result<impl IntoResponse> {
    if let Some(token) = cookie(&headers) {
        sqlx::query("DELETE FROM sessions WHERE token_hash=$1")
            .bind(crypto::hash(token))
            .execute(&state.pool)
            .await?;
    }
    Ok((
        [(
            header::SET_COOKIE,
            "artifactory_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        )],
        Json(json!({"ok":true})),
    ))
}

pub async fn me(auth: Auth) -> Json<Value> {
    Json(
        json!({"id":auth.user_id,"username":auth.username,"role":auth.role,"token":auth.token_projects.is_some()}),
    )
}
