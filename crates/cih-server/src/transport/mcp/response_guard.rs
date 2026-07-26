//! Exact, allocation-free accounting for logical MCP JSON-RPC responses.
//!
//! RMCP serializes the handler result after it leaves `ServerHandler`.  The
//! guard mirrors that JSON-RPC envelope (including the request id) into a
//! counting writer, so compatibility `content` and additive
//! `structuredContent` are both counted without building a second byte buffer.

use std::io::{self, Write};
use std::time::Duration;

use rmcp::{model::RequestId, ErrorData as McpError};
use serde::Serialize;

use crate::config::McpResponseGuardMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResponseGuardConfig {
    pub(super) target_bytes: usize,
    pub(super) max_bytes: usize,
    pub(super) mode: McpResponseGuardMode,
}

impl Default for ResponseGuardConfig {
    fn default() -> Self {
        Self {
            target_bytes: crate::config::DEFAULT_MCP_RESPONSE_TARGET_BYTES,
            max_bytes: crate::config::DEFAULT_MCP_RESPONSE_MAX_BYTES,
            mode: McpResponseGuardMode::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ResponseMeasurement {
    /// Bytes in the response that will actually leave the handler.
    pub(super) response_bytes: Option<usize>,
    /// Exact bytes in a response replaced by enforcement.
    pub(super) attempted_response_bytes: Option<usize>,
    pub(super) target_exceeded: bool,
    pub(super) max_exceeded: bool,
    pub(super) enforced: bool,
}

pub(super) struct GuardedResponse<T> {
    pub(super) result: Result<T, McpError>,
    pub(super) measurement: ResponseMeasurement,
}

impl ResponseGuardConfig {
    pub(crate) const fn new(
        target_bytes: usize,
        max_bytes: usize,
        mode: McpResponseGuardMode,
    ) -> Self {
        Self {
            target_bytes,
            max_bytes,
            mode,
        }
    }

    pub(super) fn guard<T: Serialize>(
        self,
        request_id: &RequestId,
        result: Result<T, McpError>,
    ) -> GuardedResponse<T> {
        let counted = match count_result_envelope(request_id, &result, self.max_bytes) {
            Ok(counted) => counted,
            Err(error) => {
                let result = Err(serialization_error(error));
                let response_bytes = count_result_envelope(request_id, &result, self.max_bytes)
                    .ok()
                    .map(|counted| counted.bytes);
                return GuardedResponse {
                    result,
                    measurement: ResponseMeasurement {
                        response_bytes,
                        ..ResponseMeasurement::default()
                    },
                };
            }
        };
        let target_exceeded = counted.bytes > self.target_bytes;
        let max_exceeded = counted.ceiling_exceeded;

        if max_exceeded && self.mode == McpResponseGuardMode::Enforce {
            let result = Err(result_too_large_error(counted.bytes, self));
            let response_bytes = count_result_envelope(request_id, &result, self.max_bytes)
                .ok()
                .map(|replacement| replacement.bytes);
            return GuardedResponse {
                result,
                measurement: ResponseMeasurement {
                    response_bytes,
                    attempted_response_bytes: Some(counted.bytes),
                    target_exceeded,
                    max_exceeded,
                    enforced: true,
                },
            };
        }

        GuardedResponse {
            result,
            measurement: ResponseMeasurement {
                response_bytes: Some(counted.bytes),
                attempted_response_bytes: None,
                target_exceeded,
                max_exceeded,
                enforced: false,
            },
        }
    }
}

pub(super) fn operation_deadline_error(timeout: Duration) -> McpError {
    McpError::internal_error(
        format!(
            "interactive operation exceeded its {}ms deadline; narrow or page the request",
            timeout.as_millis()
        ),
        Some(serde_json::json!({
            "code": "OPERATION_DEADLINE_EXCEEDED",
            "retryable": false,
            "timeout_ms": timeout.as_millis(),
        })),
    )
}

fn result_too_large_error(bytes: usize, config: ResponseGuardConfig) -> McpError {
    McpError::internal_error(
        "MCP response exceeds the configured safety limit; narrow or page the request",
        Some(serde_json::json!({
            "code": "RESULT_TOO_LARGE",
            "retryable": false,
            "response_bytes": bytes,
            "target_bytes": config.target_bytes,
            "max_bytes": config.max_bytes,
            "guard_mode": config.mode.as_str(),
            "truncated": false,
        })),
    )
}

fn serialization_error(error: serde_json::Error) -> McpError {
    McpError::internal_error(
        "MCP response serialization failed",
        Some(serde_json::json!({
            "code": "RESPONSE_SERIALIZATION_FAILED",
            "retryable": false,
            "detail": error.to_string(),
        })),
    )
}

#[derive(Serialize)]
struct SuccessEnvelope<'a, T> {
    jsonrpc: &'static str,
    id: &'a RequestId,
    result: &'a T,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    jsonrpc: &'static str,
    id: &'a RequestId,
    error: &'a McpError,
}

fn count_result_envelope<T: Serialize>(
    request_id: &RequestId,
    result: &Result<T, McpError>,
    ceiling: usize,
) -> Result<CountedBytes, serde_json::Error> {
    match result {
        Ok(result) => count_serialized(
            &SuccessEnvelope {
                jsonrpc: "2.0",
                id: request_id,
                result,
            },
            ceiling,
        ),
        Err(error) => count_serialized(
            &ErrorEnvelope {
                jsonrpc: "2.0",
                id: request_id,
                error,
            },
            ceiling,
        ),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CountedBytes {
    bytes: usize,
    ceiling_exceeded: bool,
}

/// A serializer sink that retains no response data. It keeps counting after
/// crossing `ceiling`, which makes warn/measure mode exact while still exposing
/// whether the configured cap was crossed.
struct CappedCountingWriter {
    bytes: usize,
    ceiling: usize,
    ceiling_exceeded: bool,
}

impl CappedCountingWriter {
    fn new(ceiling: usize) -> Self {
        Self {
            bytes: 0,
            ceiling,
            ceiling_exceeded: false,
        }
    }
}

impl Write for CappedCountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::other("serialized response byte count overflow"))?;
        self.ceiling_exceeded |= self.bytes > self.ceiling;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn count_serialized<T: Serialize>(
    value: &T,
    ceiling: usize,
) -> Result<CountedBytes, serde_json::Error> {
    let mut writer = CappedCountingWriter::new(ceiling);
    serde_json::to_writer(&mut writer, value)?;
    Ok(CountedBytes {
        bytes: writer.bytes,
        ceiling_exceeded: writer.ceiling_exceeded,
    })
}

#[cfg(test)]
mod tests {
    use rmcp::model::{CallToolResult, Content, JsonRpcError, JsonRpcResponse, JsonRpcVersion2_0};
    use rmcp::{model::NumberOrString, ErrorData as McpError};

    use super::{count_result_envelope, operation_deadline_error, ResponseGuardConfig};
    use crate::config::McpResponseGuardMode;

    fn request_id(value: &str) -> NumberOrString {
        NumberOrString::String(value.into())
    }

    fn duplicated_result(size: usize) -> CallToolResult {
        let structured = serde_json::json!({"payload": "x".repeat(size)});
        let content = Content::json(&structured).unwrap();
        let mut result = CallToolResult::success(vec![content]);
        result.structured_content = Some(structured);
        result
    }

    #[test]
    fn counting_matches_rmcp_success_envelope_and_includes_request_id() {
        let id = request_id("request-with-a-long-id");
        let result = duplicated_result(128);
        let counted =
            count_result_envelope(&id, &Ok::<_, McpError>(result.clone()), usize::MAX).unwrap();
        let actual = serde_json::to_vec(&JsonRpcResponse {
            jsonrpc: JsonRpcVersion2_0,
            id: id.clone(),
            result,
        })
        .unwrap();
        assert_eq!(counted.bytes, actual.len());

        let short = count_result_envelope(
            &request_id("x"),
            &Ok::<_, McpError>(duplicated_result(128)),
            usize::MAX,
        )
        .unwrap();
        assert!(counted.bytes > short.bytes);
    }

    #[test]
    fn compatibility_and_structured_payloads_are_both_counted() {
        let id = request_id("duplicate");
        let duplicated = duplicated_result(512);
        let duplicated_bytes =
            count_result_envelope(&id, &Ok::<_, McpError>(duplicated.clone()), usize::MAX)
                .unwrap()
                .bytes;
        let mut compatibility_only = duplicated;
        compatibility_only.structured_content = None;
        let compatibility_bytes =
            count_result_envelope(&id, &Ok::<_, McpError>(compatibility_only), usize::MAX)
                .unwrap()
                .bytes;
        assert!(duplicated_bytes > compatibility_bytes + 500);
    }

    #[test]
    fn enforcement_accepts_exact_limit_and_replaces_one_byte_over() {
        let id = request_id("boundary");
        let result = duplicated_result(1200);
        let exact = count_result_envelope(&id, &Ok::<_, McpError>(result.clone()), usize::MAX)
            .unwrap()
            .bytes;
        let accepted = ResponseGuardConfig {
            target_bytes: exact,
            max_bytes: exact,
            mode: McpResponseGuardMode::Enforce,
        }
        .guard(&id, Ok::<_, McpError>(result.clone()));
        assert!(accepted.result.is_ok());
        assert!(!accepted.measurement.enforced);
        assert_eq!(accepted.measurement.response_bytes, Some(exact));

        let rejected = ResponseGuardConfig {
            target_bytes: exact - 1,
            max_bytes: exact - 1,
            mode: McpResponseGuardMode::Enforce,
        }
        .guard(&id, Ok::<_, McpError>(result));
        let error = rejected.result.unwrap_err();
        assert_eq!(
            error.data.as_ref().unwrap()["code"],
            serde_json::json!("RESULT_TOO_LARGE")
        );
        assert_eq!(
            error.data.as_ref().unwrap()["response_bytes"],
            serde_json::json!(exact)
        );
        assert_eq!(rejected.measurement.attempted_response_bytes, Some(exact));
        assert!(rejected.measurement.enforced);
    }

    #[test]
    fn warn_mode_measures_an_oversized_success_without_mutating_it() {
        let id = request_id("warn");
        let guarded = ResponseGuardConfig {
            target_bytes: 1,
            max_bytes: 2,
            mode: McpResponseGuardMode::Warn,
        }
        .guard(&id, Ok::<_, McpError>(duplicated_result(128)));
        assert!(guarded.result.is_ok());
        assert!(guarded.measurement.target_exceeded);
        assert!(guarded.measurement.max_exceeded);
        assert!(!guarded.measurement.enforced);
    }

    #[test]
    fn counting_matches_rmcp_error_envelope() {
        let id = request_id("error-id");
        let error = McpError::internal_error(
            "dependency unavailable",
            Some(serde_json::json!({"code": "UNAVAILABLE", "retryable": true})),
        );
        let counted =
            count_result_envelope::<serde_json::Value>(&id, &Err(error.clone()), usize::MAX)
                .unwrap();
        let actual = serde_json::to_vec(&JsonRpcError {
            jsonrpc: JsonRpcVersion2_0,
            id,
            error,
        })
        .unwrap();
        assert_eq!(counted.bytes, actual.len());
    }

    #[test]
    fn operation_timeout_is_a_bounded_measured_error_response() {
        let id = request_id("timeout-id");
        let error = operation_deadline_error(std::time::Duration::from_millis(15_000));
        assert_eq!(
            error.data.as_ref().unwrap()["code"],
            serde_json::json!("OPERATION_DEADLINE_EXCEEDED")
        );
        let counted = count_result_envelope::<serde_json::Value>(&id, &Err(error), 1024).unwrap();
        assert!(!counted.ceiling_exceeded);
        assert!(counted.bytes > 100);
    }
}
