use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

pub type Result<T> = std::result::Result<T, ApiError>;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("authentication required")]
    Unauthorized,
    #[error("permission denied")]
    Forbidden,
    #[error("resource not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        if let sqlx::Error::Database(database) = &error {
            if database.is_unique_violation() {
                return Self::Conflict("resource already exists".into());
            }
            if database.is_foreign_key_violation() || database.is_check_violation() {
                return Self::BadRequest("invalid resource reference or value".into());
            }
        }
        if matches!(error, sqlx::Error::RowNotFound) {
            Self::NotFound
        } else {
            Self::Internal(error.into())
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Internal(error) => {
                tracing::error!(error = %error, "request failed");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        let message = if status.is_server_error() {
            "internal server error".to_owned()
        } else {
            self.to_string()
        };
        (status, Json(json!({"error": message}))).into_response()
    }
}
