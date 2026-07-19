//! End-to-end MCP dispatch tests: drive real `call_tool` requests through the
//! actual `ServerHandler` + tool router over an in-memory transport pair (rmcp's
//! own test pattern, `rmcp/tests/test_tool_macros.rs`). This verifies the
//! request → dispatch → response envelope that the registration guard in
//! `app::tests` can't.
//!
//! Hermetic: `FalkorStore::connect` is lazy (connects on first query), and none
//! of the tools dispatched here reach a graph query — they fail at
//! router/arg-validation, or `list_repos` only reads the registry — so no live
//! FalkorDB is required and this runs in the normal suite.

use std::sync::Arc;
use std::time::Duration;

use cih_graph_store::GraphStore;
use rmcp::model::{CallToolRequestParam, ClientInfo};
use rmcp::{ClientHandler, ServiceExt};

use super::CihServer;
use crate::{files, wiki};

#[derive(Clone, Default)]
struct DummyClient;

impl ClientHandler for DummyClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

type TestClient = rmcp::service::RunningService<rmcp::RoleClient, DummyClient>;

/// Build a `CihServer` (lazy Falkor connection — no live DB) and serve it over an
/// in-memory duplex pair; return a connected client.
async fn serve_test_server() -> TestClient {
    let store: Arc<dyn GraphStore> = Arc::new(
        cih_falkor::FalkorStore::connect("redis://127.0.0.1:6380", "cih_dispatch_test")
            .expect("lazy FalkorStore connect"),
    );
    let server = CihServer::new(
        store,
        None,
        None,
        "cih_dispatch_test".into(),
        None,
        "redis://127.0.0.1:6380".into(),
        (4, Duration::from_secs(5)),
        files::ReadFileLimits {
            max_bytes: 1 << 20,
            max_lines: 2000,
        },
        wiki::WikiSearchState::new("cih_dispatch_test".into()),
    );
    let (server_t, client_t) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _ = server.serve(server_t).await?.waiting().await;
        anyhow::Ok(())
    });
    DummyClient
        .serve(client_t)
        .await
        .expect("client serve over duplex")
}

fn empty_args() -> serde_json::Map<String, serde_json::Value> {
    serde_json::Map::new()
}

#[tokio::test]
async fn dispatch_unknown_tool_returns_error() {
    let client = serve_test_server().await;
    let res = client
        .call_tool(CallToolRequestParam {
            name: "no_such_tool".into(),
            arguments: None,
        })
        .await;
    assert!(
        res.is_err(),
        "unknown tool must dispatch to an error, got {res:?}"
    );
    client.cancel().await.ok();
}

#[tokio::test]
async fn dispatch_call_that_cannot_resolve_returns_error() {
    // `impact` with empty args can't proceed (missing symbol / unresolvable repo)
    // — the error must come back as a well-formed envelope, not a panic or hang.
    let client = serve_test_server().await;
    let res = client
        .call_tool(CallToolRequestParam {
            name: "impact".into(),
            arguments: Some(empty_args()),
        })
        .await;
    assert!(
        res.is_err(),
        "unresolvable call must return an error, got {res:?}"
    );
    client.cancel().await.ok();
}

#[tokio::test]
async fn dispatch_list_repos_returns_success_envelope() {
    // `list_repos` only reads the registry (read-only); assert the success
    // envelope shape, independent of registry contents.
    let client = serve_test_server().await;
    let res = client
        .call_tool(CallToolRequestParam {
            name: "list_repos".into(),
            arguments: Some(empty_args()),
        })
        .await
        .expect("list_repos should dispatch successfully");
    assert!(
        !res.is_error.unwrap_or(false),
        "list_repos should be a success result"
    );
    assert!(!res.content.is_empty(), "expected a JSON content payload");
    client.cancel().await.ok();
}
