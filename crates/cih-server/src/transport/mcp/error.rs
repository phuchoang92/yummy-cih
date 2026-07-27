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
        AppError::GraphUnavailable {
            code,
            message,
            retryable,
            retry_after_ms,
        } => {
            tracing::error!(code, error = %message, retryable, retry_after_ms, "graph dependency unavailable");
            McpError::internal_error(
                format!(
                    "graph store unavailable ({code}){}",
                    if retryable { "; retry shortly" } else { "" }
                ),
                Some(serde_json::json!({
                    "dependency": "graph_store",
                    "code": code,
                    "retryable": retryable,
                    "retry_after_ms": retry_after_ms,
                })),
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
