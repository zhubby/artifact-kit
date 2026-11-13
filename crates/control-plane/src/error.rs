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
    #[error("{0}")]
    BadGateway(String),
    #[error("{0}")]
    GatewayTimeout(String),
    #[error("too many requests")]
    TooManyRequests(u64),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::BadGateway(_) => StatusCode::BAD_GATEWAY,
            Self::GatewayTimeout(_) => StatusCode::GATEWAY_TIMEOUT,
            Self::TooManyRequests(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::Internal(error) => {
                tracing::error!(error = %error, "request failed");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        let message = if matches!(&self, Self::Internal(_)) {
            "internal server error".to_owned()
        } else {
            self.to_string()
        };
        let mut response = (status, Json(json!({"error": message}))).into_response();
        if let Self::TooManyRequests(seconds) = self {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                seconds.to_string().parse().unwrap(),
            );
        }
        response
    }
}
