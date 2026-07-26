//! Transport-independent domain and application errors.

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
    /// Classified graph dependency failure. Keeping this separate from generic
    /// `Unavailable` preserves backend state and retry guidance through MCP and
    /// HTTP adapters without parsing an error string.
    GraphUnavailable {
        code: &'static str,
        message: String,
        retryable: bool,
        retry_after_ms: Option<u64>,
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
            Self::GraphUnavailable { code, message, .. } => {
                write!(f, "graph store unavailable ({code}): {message}")
            }
        }
    }
}

impl std::error::Error for AppError {}

impl AppError {
    pub(crate) fn from_graph_store(
        error: cih_graph_store::GraphStoreError,
        not_found_entity: &'static str,
    ) -> Self {
        use cih_graph_store::GraphStoreError;

        match error {
            GraphStoreError::NotFound(key) => Self::NotFound {
                entity: not_found_entity,
                key,
            },
            GraphStoreError::InvalidInput(message) => Self::InvalidInput {
                field: "graph query",
                message,
            },
            other => {
                let retry = other.retry_metadata();
                let code = match &other {
                    GraphStoreError::Loading { .. } => "BACKEND_LOADING",
                    GraphStoreError::Overloaded { .. } => "BACKEND_OVERLOADED",
                    GraphStoreError::ExecutionTimeout { .. } => "BACKEND_TIMEOUT",
                    GraphStoreError::Unavailable { .. } | GraphStoreError::Backend(_) => {
                        "BACKEND_UNAVAILABLE"
                    }
                    GraphStoreError::Index { .. } => "INDEX_UNAVAILABLE",
                    GraphStoreError::Unimplemented(_) => "BACKEND_UNIMPLEMENTED",
                    GraphStoreError::Other(_) => "BACKEND_ERROR",
                    GraphStoreError::NotFound(_) | GraphStoreError::InvalidInput(_) => {
                        unreachable!("domain errors returned above")
                    }
                };
                Self::GraphUnavailable {
                    code,
                    message: other.to_string(),
                    retryable: retry
                        .map(|metadata| metadata.retryable)
                        .unwrap_or(matches!(other, GraphStoreError::Backend(_))),
                    retry_after_ms: retry.and_then(|metadata| metadata.retry_after_ms),
                }
            }
        }
    }
}
