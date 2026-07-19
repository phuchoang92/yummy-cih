//! Live-FalkorDB integration test — the sole graph backend was previously
//! covered only by pure-helper unit tests (`falkor.rs`), leaving the actual
//! read/write paths untested. This round-trips a tiny graph through the real DB.
//!
//! `#[ignore]`d so `cargo test --workspace` stays hermetic. Run against a
//! FalkorDB reachable at `FALKOR_URL` (default `redis://127.0.0.1:6380`):
//!
//! ```text
//! cargo test -p cih-falkor --test falkor_integration -- --ignored
//! ```

use std::sync::atomic::{AtomicU64, Ordering};

use cih_core::{Edge, EdgeKind, GraphArtifacts, Node, NodeId, NodeKind, Range, VersionId};
use cih_falkor::FalkorStore;
use cih_graph_store::{Direction, GraphStore};

const CALLER_ID: &str = "Method:com.acme.Foo#caller/0";
const CALLEE_ID: &str = "Method:com.acme.Bar#callee/0";

fn falkor_url() -> String {
    std::env::var("FALKOR_URL").unwrap_or_else(|_| "redis://127.0.0.1:6380".to_string())
}

fn node(id: &str, name: &str, file: &str) -> Node {
    Node {
        id: NodeId::new(id),
        kind: NodeKind::Method,
        name: name.to_string(),
        qualified_name: None,
        file: file.to_string(),
        range: Range::default(),
        props: None,
    }
}

#[tokio::test]
#[ignore = "requires a live FalkorDB (FALKOR_URL, default redis://127.0.0.1:6380); run with --ignored"]
async fn round_trips_nodes_edges_and_serves_queries() {
    // Unique graph key so parallel/re-runs never clash on shared DB state.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let graph_key = format!(
        "cih_it_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );

    let store = FalkorStore::connect(&falkor_url(), graph_key.as_str()).expect("connect FalkorDB");
    store.drop_graph().await.expect("clean slate");

    // Minimal graph: caller --Calls--> callee, in two files.
    let caller = node(CALLER_ID, "caller", "com/acme/Foo.java");
    let callee = node(CALLEE_ID, "callee", "com/acme/Bar.java");
    let edge = Edge::new(
        caller.id.clone(),
        callee.id.clone(),
        EdgeKind::Calls,
        1.0,
        "test".to_string(),
    );

    let dir = std::env::temp_dir().join(format!("{graph_key}-artifacts"));
    let artifacts = GraphArtifacts::write(
        &dir,
        VersionId::new("v1"),
        &[caller.clone(), callee.clone()],
        &[edge],
    )
    .expect("write artifacts");

    store.bulk_load(&artifacts).await.expect("bulk_load");
    // Recreate indices dropped by the bulk path, mirroring a fresh server connect.
    store.ensure_schema().await.expect("ensure_schema");

    // candidates_by_name resolves the callee by short name.
    let cands = store
        .candidates_by_name("callee", 10)
        .await
        .expect("candidates_by_name");
    assert!(
        cands.iter().any(|n| n.id.to_string() == CALLEE_ID),
        "callee not found via candidates_by_name: {cands:?}"
    );

    // Upstream impact from the callee reports the caller in the blast radius.
    let impact = store
        .impact(&callee.id, Direction::Upstream, 4)
        .await
        .expect("impact");
    assert!(
        impact
            .affected
            .iter()
            .any(|n| n.id.to_string() == CALLER_ID),
        "caller not in upstream impact of callee"
    );

    // nodes_in_files maps a file back to its nodes.
    let in_file = store
        .nodes_in_files(&["com/acme/Foo.java".to_string()])
        .await
        .expect("nodes_in_files");
    assert!(
        in_file.iter().any(|n| n.id.to_string() == CALLER_ID),
        "caller not found via nodes_in_files"
    );

    // Cleanup: remove the throwaway graph and artifacts dir.
    store.drop_graph().await.expect("drop_graph cleanup");
    let _ = std::fs::remove_dir_all(&dir);
}
