use crate::error::ApiError;

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
