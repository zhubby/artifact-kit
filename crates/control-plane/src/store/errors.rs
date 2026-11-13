use crate::error::ApiError;
use artifactory_database::{Constraint, Error};
impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        match artifactory_database::constraint(&error) {
            Some(Constraint::Unique) => Self::Conflict("resource already exists".into()),
            Some(Constraint::ForeignKey | Constraint::Check | Constraint::NotNull) => {
                Self::BadRequest("invalid resource reference or value".into())
            }
            None if error.is_not_found() => Self::NotFound,
            None => Self::Internal(error.into()),
        }
    }
}
impl From<toasty::Error> for ApiError {
    fn from(error: toasty::Error) -> Self {
        Error::from(error).into()
    }
}
