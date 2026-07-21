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
    pub(crate) response_bytes: Option<usize>,
    pub(crate) completeness: Option<String>,
    pub(crate) error_kind: Option<RequestErrorKind>,
}
