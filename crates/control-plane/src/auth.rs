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
    extract::{FromRequestParts, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header, request::Parts},
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::{Value, json};
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
            return state
                .store
                .authenticate_token(&crypto::hash(token))
                .await?
                .ok_or(ApiError::Unauthorized);
        }
        if ![Method::GET, Method::HEAD, Method::OPTIONS].contains(&parts.method) {
            csrf(&parts.headers, state)?;
        }
        let token = cookie(&parts.headers).ok_or(ApiError::Unauthorized)?;
        state
            .store
            .authenticate_session(&crypto::hash(token))
            .await?
            .ok_or(ApiError::Unauthorized)
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

pub async fn registration_config(State(state): State<AppState>) -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"registration_enabled":state.settings.registration_enabled})),
    )
}

pub async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    input: std::result::Result<Json<Login>, JsonRejection>,
) -> Result<impl IntoResponse> {
    csrf(&headers, &state)?;
    if !state.settings.registration_enabled {
        return Err(ApiError::Forbidden);
    }
    let Json(input) =
        input.map_err(|_| ApiError::BadRequest("invalid registration fields".into()))?;
    let username = input.username.trim();
    if username.contains('\0') {
        return Err(ApiError::BadRequest("invalid registration fields".into()));
    }
    if username.is_empty() || username.len() > 100 {
        return Err(ApiError::BadRequest(
            "username must contain 1..100 bytes".into(),
        ));
    }
    if !(12..=256).contains(&input.password.len()) {
        return Err(ApiError::BadRequest(
            "password must contain 12..256 bytes".into(),
        ));
    }
    state.store.reserve_registration().await?;
    let password = input.password;
    let encoded = tokio::task::spawn_blocking(move || password_hash(&password))
        .await
        .map_err(anyhow::Error::from)??;
    let user_id = Uuid::new_v4();
    let token = crypto::token();
    state
        .store
        .register_viewer(user_id, username, &encoded, &crypto::hash(&token))
        .await?;
    Ok((
        StatusCode::CREATED,
        [(
            header::SET_COOKIE,
            session_cookie(&token, state.settings.insecure_dev),
        )],
        Json(json!({"id":user_id,"username":username,"role":"viewer"})),
    ))
}

fn session_cookie(token: &str, insecure_dev: bool) -> HeaderValue {
    let secure = if insecure_dev { "" } else { "; Secure" };
    HeaderValue::from_str(&format!(
        "artifactory_session={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400{secure}"
    ))
    .unwrap()
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
    let recent = state.store.recent_login_failures(&input.username).await?;
    if recent >= 10 {
        return Err(ApiError::Unauthorized);
    }
    let row = state.store.login_account(&input.username).await?;
    let valid = if let Some(row) = &row {
        let encoded = row.password_hash.clone();
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
    let user_id = row.id;
    let token = crypto::token();
    state
        .store
        .create_session(&crypto::hash(&token), user_id)
        .await?;
    state.audit(user_id, "login", user_id, json!({})).await?;
    Ok((
        [(
            header::SET_COOKIE,
            session_cookie(&token, state.settings.insecure_dev),
        )],
        Json(json!({"id":user_id,"username":input.username,"role":row.role})),
    ))
}

pub async fn logout(
    State(state): State<AppState>,
    _auth: Auth,
    headers: HeaderMap,
) -> Result<impl IntoResponse> {
    if let Some(token) = cookie(&headers) {
        state.store.remove_session(&crypto::hash(token)).await?;
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
