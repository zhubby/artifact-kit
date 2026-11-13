//! Shared build request rules.
use crate::error::{ApiError, Result};

pub(crate) fn validate_release_version(version: &str) -> Result<()> {
    if version.is_empty() || version.len() > 100 || version.chars().any(char::is_control) {
        return Err(ApiError::BadRequest("invalid release version".into()));
    }
    Ok(())
}
