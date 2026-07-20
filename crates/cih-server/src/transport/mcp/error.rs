//! Mapping between application results and MCP protocol results.

use rmcp::{
    model::{CallToolResult, Content},
    ErrorData as McpError,
};

use crate::domain::error::AppError;

pub(crate) fn app_error_to_mcp(error: AppError) -> McpError {
    match error {
        AppError::InvalidInput { field, message } => {
            McpError::invalid_params(format!("invalid {field}: {message}"), None)
        }
        AppError::NotFound { entity, key } => {
            McpError::invalid_params(format!("{entity} '{key}' not found"), None)
        }
        AppError::Unavailable {
            dependency,
            message,
            retryable,
        } => {
            tracing::error!(dependency, error = %message, retryable, "application dependency unavailable");
            McpError::internal_error(
                format!(
                    "{dependency} unavailable{}",
                    if retryable { "; retry shortly" } else { "" }
                ),
                None,
            )
        }
    }
}

pub(crate) fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let content =
        Content::json(value).map_err(|error| McpError::internal_error(error.to_string(), None))?;
    Ok(CallToolResult::success(vec![content]))
}

pub(crate) fn text_result(value: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(value)]))
}
