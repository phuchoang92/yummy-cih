//! Bounded operational event vocabulary shared by transports and adapters.

use serde::Serialize;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RequestTransport {
    Mcp,
    Http,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RequestErrorKind {
    Protocol,
    Timeout,
    Overload,
    Dependency,
    ResponseLimit,
    Internal,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RequestCompletion {
    pub(crate) request_id: String,
    pub(crate) transport: RequestTransport,
    pub(crate) capability: String,
    pub(crate) repository_id: Option<String>,
    pub(crate) duration_ms: u64,
    pub(crate) queue_wait_ms: Option<u64>,
    pub(crate) result_count: Option<usize>,
    /// Exact bytes in the logical response envelope that is emitted.
    pub(crate) response_bytes: Option<usize>,
    /// Exact bytes in an oversized envelope replaced by the response guard.
    pub(crate) attempted_response_bytes: Option<usize>,
    pub(crate) response_target_exceeded: bool,
    pub(crate) response_max_exceeded: bool,
    pub(crate) response_guard_enforced: bool,
    pub(crate) completeness: Option<String>,
    pub(crate) error_kind: Option<RequestErrorKind>,
}
