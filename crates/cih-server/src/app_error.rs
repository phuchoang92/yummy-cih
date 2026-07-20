//! Transport-independent application errors.

use std::fmt;

#[derive(Clone, Debug)]
pub(crate) enum AppError {
    InvalidInput {
        field: &'static str,
        message: String,
    },
    NotFound {
        entity: &'static str,
        key: String,
    },
    Unavailable {
        dependency: &'static str,
        message: String,
        retryable: bool,
    },
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput { field, message } => write!(f, "invalid {field}: {message}"),
            Self::NotFound { entity, key } => write!(f, "{entity} '{key}' not found"),
            Self::Unavailable {
                dependency,
                message,
                ..
            } => write!(f, "{dependency} unavailable: {message}"),
        }
    }
}

impl std::error::Error for AppError {}
