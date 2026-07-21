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
    let structured = serde_json::to_value(value)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    let content = Content::json(&structured)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    let mut result = CallToolResult::success(vec![content]);
    // Additive MCP structured content keeps the existing text content intact
    // while making response accounting and typed clients reliable.
    result.structured_content = Some(structured);
    Ok(result)
}

pub(crate) fn text_result(value: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(value)]))
}

/// Preserve a legacy text payload while exposing a richer additive structured
/// result to clients that support MCP structured content.
pub(crate) fn json_result_compatible<L, S>(
    legacy: &L,
    structured: &S,
) -> Result<CallToolResult, McpError>
where
    L: serde::Serialize,
    S: serde::Serialize,
{
    let content =
        Content::json(legacy).map_err(|error| McpError::internal_error(error.to_string(), None))?;
    let structured_content = serde_json::to_value(structured)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    let mut result = CallToolResult::success(vec![content]);
    result.structured_content = Some(structured_content);
    Ok(result)
}
