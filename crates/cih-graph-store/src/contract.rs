//! Backend-neutral contract suite: the behaviors every `GraphStore` adapter
//! must exhibit, parameterized over a store constructor. An adapter passes by
//! running [`run_contract_suite`] against a live instance of its backend (see
//! `cih-falkor/tests/falkor_integration.rs`); a new backend is not considered
//! wired in until this suite is green.
//!
//! The constructor is **key-parameterized** — `mk(graph_key)` — because the
//! publish test must build a store on a staging key, publish, then construct a
//! *second* store for the destination key against the same backend instance.
//!
//! Gated behind the `test-support` feature so production builds don't carry
//! fixture code.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cih_core::{
    Edge, EdgeKind, GraphArtifacts, GraphDelta, Node, NodeId, NodeKind, Range, VersionId,
};

use crate::{Direction, GraphStore, LoadObserver};

type MkResult = anyhow::Result<Arc<dyn GraphStore>>;

const CALLER_ID: &str = "Method:com.acme.Foo#caller/0";
const CALLEE_ID: &str = "Method:com.acme.Bar#callee/0";
const HANDLER_ID: &str = "Method:com.acme.Api#getThings/0";
const ROUTE_ID: &str = "Route:GET /api/things";
const CALLER_FILE: &str = "com/acme/Foo.java";
const CALLEE_FILE: &str = "com/acme/Bar.java";
const API_FILE: &str = "com/acme/Api.java";

fn method(id: &str, name: &str, file: &str) -> Node {
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

fn route() -> Node {
    Node {
        id: NodeId::new(ROUTE_ID),
        kind: NodeKind::Route,
        name: "GET /api/things".to_string(),
        qualified_name: None,
        file: API_FILE.to_string(),
        range: Range::default(),
        props: Some(serde_json::json!({
            "path": "/api/things",
            "httpMethod": "GET",
        })),
    }
}

/// caller --CALLS--> callee, handler --HANDLES_ROUTE--> route,
/// handler --CALLS--> caller (so the route reaches the chain).
fn fixture_nodes_edges() -> (Vec<Node>, Vec<Edge>) {
    let caller = method(CALLER_ID, "caller", CALLER_FILE);
    let callee = method(CALLEE_ID, "callee", CALLEE_FILE);
    let handler = method(HANDLER_ID, "getThings", API_FILE);
    let route = route();
    let edges = vec![
        Edge::new(
            caller.id.clone(),
            callee.id.clone(),
            EdgeKind::Calls,
            1.0,
            "contract".to_string(),
        ),
        Edge::new(
            handler.id.clone(),
            caller.id.clone(),
            EdgeKind::Calls,
            1.0,
            "contract".to_string(),
        ),
        Edge::new(
            handler.id.clone(),
            route.id.clone(),
            EdgeKind::HandlesRoute,
            1.0,
            "contract".to_string(),
        ),
    ];
    (vec![caller, callee, handler, route], edges)
}

/// Write the fixture as JSONL artifacts in a unique temp dir.
fn write_fixture(tag: &str) -> anyhow::Result<(GraphArtifacts, std::path::PathBuf)> {
    let (nodes, edges) = fixture_nodes_edges();
    let dir = std::env::temp_dir().join(format!("{tag}-artifacts"));
    let artifacts = GraphArtifacts::write(&dir, VersionId::new("contract-v1"), &nodes, &edges)?;
    Ok((artifacts, dir))
}

/// Unique namespace per suite run so parallel/re-runs never clash on shared
/// backend state.
fn namespace() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "cihct_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Records observer callbacks so the suite can assert ordering.
#[derive(Default)]
struct RecordingObserver {
    events: Mutex<Vec<(&'static str, u64)>>,
}

impl LoadObserver for RecordingObserver {
    fn nodes_loaded(&self, count: u64) {
        self.events.lock().unwrap().push(("nodes", count));
    }
    fn edges_loaded(&self, count: u64) {
        self.events.lock().unwrap().push(("edges", count));
    }
    fn indexes_built(&self) {
        self.events.lock().unwrap().push(("indexes", 0));
    }
}

/// Run the full backend contract against stores built by `mk(graph_key)`.
///
/// `mk` must return a store bound to the given key on the same backend
/// instance each time. Panics (assert) on contract violations; returns `Err`
/// only for infrastructure failures (fixture I/O, store construction).
pub async fn run_contract_suite<F>(mk: F) -> anyhow::Result<()>
where
    F: Fn(&str) -> MkResult,
{
    let ns = namespace();
    round_trip_reads(&mk, &ns).await?;
    incremental_upsert(&mk, &ns).await?;
    publish_and_drop(&mk, &ns).await?;
    observed_load(&mk, &ns).await?;
    Ok(())
}

/// bulk_load → summary/point-lookup/traversal/route round-trip.
async fn round_trip_reads<F>(mk: &F, ns: &str) -> anyhow::Result<()>
where
    F: Fn(&str) -> MkResult,
{
    let key = format!("{ns}_rt");
    let (artifacts, dir) = write_fixture(&key)?;
    let store = mk(&key)?;
    let _ = store.drop_graph().await;

    let stats = store.bulk_load(&artifacts).await.expect("bulk_load");
    assert_eq!(stats.nodes, 4, "fixture node count");
    assert_eq!(stats.edges, 3, "fixture edge count");
    // Mirror a fresh server connect (bulk fast paths may rebuild indexes).
    store.ensure_schema().await.expect("ensure_schema");

    let summary = store.graph_summary().await.expect("graph_summary");
    assert_eq!(summary.total_nodes, 4, "summary total_nodes");
    assert_eq!(summary.total_edges, 3, "summary total_edges");

    let callee_id = NodeId::new(CALLEE_ID);
    let node = store
        .get_node(&callee_id)
        .await
        .expect("get_node")
        .expect("callee exists");
    assert_eq!(node.name, "callee");
    assert_eq!(node.file, CALLEE_FILE);

    // `src`/`dst` must reflect the STORED direction regardless of query
    // direction: upstream of callee still reports caller→callee.
    let nbrs = store
        .neighbors(&callee_id, Direction::Upstream, &[EdgeKind::Calls])
        .await
        .expect("neighbors");
    assert!(
        nbrs.iter()
            .any(|e| e.src.as_str() == CALLER_ID && e.dst.as_str() == CALLEE_ID),
        "caller→callee edge visible upstream with stored orientation: {nbrs:?}"
    );
    let down = store
        .neighbors(
            &NodeId::new(CALLER_ID),
            Direction::Downstream,
            &[EdgeKind::Calls],
        )
        .await
        .expect("neighbors downstream");
    assert!(
        down.iter()
            .any(|e| e.src.as_str() == CALLER_ID && e.dst.as_str() == CALLEE_ID),
        "caller→callee edge visible downstream with stored orientation: {down:?}"
    );

    // Impact must report members AND correct depths (values, not just counts —
    // catches off-by-one dialect bugs like 1-based list indexing).
    let impact = store
        .impact(&callee_id, Direction::Upstream, 4)
        .await
        .expect("impact");
    let caller_hit = impact
        .affected
        .iter()
        .find(|n| n.id.as_str() == CALLER_ID)
        .expect("caller in upstream impact");
    assert_eq!(caller_hit.depth, 1, "caller is one hop upstream");
    let handler_hit = impact
        .affected
        .iter()
        .find(|n| n.id.as_str() == HANDLER_ID)
        .expect("handler in upstream impact (2 hops)");
    assert_eq!(handler_hit.depth, 2, "handler is two hops upstream");

    let chains = store
        .call_chain(&NodeId::new(HANDLER_ID), &callee_id, 5)
        .await
        .expect("call_chain");
    assert!(
        chains.iter().any(|p| {
            p.nodes.first().map(NodeId::as_str) == Some(HANDLER_ID)
                && p.nodes.last().map(NodeId::as_str) == Some(CALLEE_ID)
        }),
        "handler→caller→callee chain found: {chains:?}"
    );

    let ctx = store
        .context(&NodeId::new(CALLER_ID))
        .await
        .expect("context");
    assert!(
        ctx.callers.iter().any(|n| n.id.as_str() == HANDLER_ID),
        "context callers include handler"
    );
    assert!(
        ctx.callees.iter().any(|n| n.id.as_str() == CALLEE_ID),
        "context callees include callee"
    );

    let routes = store.route_map(None, 50).await.expect("route_map");
    let r = routes
        .iter()
        .find(|r| r.path == "/api/things")
        .expect("route present in route_map");
    assert_eq!(r.http_method, "GET");
    assert_eq!(r.handler_id.as_str(), HANDLER_ID);

    let cands = store
        .candidates_by_name("callee", 10)
        .await
        .expect("candidates_by_name");
    assert!(
        cands.iter().any(|n| n.id.as_str() == CALLEE_ID),
        "callee found by short name"
    );

    let in_files = store
        .nodes_in_files(&[CALLER_FILE.to_string()])
        .await
        .expect("nodes_in_files");
    assert!(
        in_files.iter().any(|n| n.id.as_str() == CALLER_ID),
        "caller found via nodes_in_files"
    );

    store.drop_graph().await.expect("drop_graph cleanup");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// upsert_incremental: changed-file delete + reload semantics.
async fn incremental_upsert<F>(mk: &F, ns: &str) -> anyhow::Result<()>
where
    F: Fn(&str) -> MkResult,
{
    let key = format!("{ns}_inc");
    let (artifacts, dir) = write_fixture(&key)?;
    let store = mk(&key)?;
    let _ = store.drop_graph().await;
    store.bulk_load(&artifacts).await.expect("bulk_load");
    store.ensure_schema().await.expect("ensure_schema");

    // Foo.java changed: `caller` was renamed to `caller2` (new id).
    let new_caller = method("Method:com.acme.Foo#caller2/0", "caller2", CALLER_FILE);
    let delta = GraphDelta {
        changed_files: vec![CALLER_FILE.to_string()],
        removed_files: vec![],
        nodes: vec![new_caller.clone()],
        edges: vec![Edge::new(
            new_caller.id.clone(),
            NodeId::new(CALLEE_ID),
            EdgeKind::Calls,
            1.0,
            "contract".to_string(),
        )],
    };
    store
        .upsert_incremental(&delta)
        .await
        .expect("upsert_incremental");

    assert!(
        store
            .get_node(&NodeId::new(CALLER_ID))
            .await
            .expect("get_node old")
            .is_none(),
        "old node from the changed file was deleted"
    );
    assert!(
        store
            .get_node(&new_caller.id)
            .await
            .expect("get_node new")
            .is_some(),
        "replacement node was loaded"
    );
    assert!(
        store
            .get_node(&NodeId::new(CALLEE_ID))
            .await
            .expect("get_node untouched")
            .is_some(),
        "node in an untouched file survived the delta"
    );

    store.drop_graph().await.expect("drop_graph cleanup");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// publish_to + drop_graph: the staging→live swap. Encodes the port guarantee:
/// after `publish_to(dest)` returns, dropping the staging graph must not affect
/// the published data (the engine does exactly this after every load).
async fn publish_and_drop<F>(mk: &F, ns: &str) -> anyhow::Result<()>
where
    F: Fn(&str) -> MkResult,
{
    let live_key = format!("{ns}_pub");
    let staging_key = format!("{live_key}-staging");
    let (artifacts, dir) = write_fixture(&live_key)?;

    let staging = mk(&staging_key)?;
    let _ = staging.drop_graph().await;
    staging
        .bulk_load(&artifacts)
        .await
        .expect("bulk_load staging");
    staging.publish_to(&live_key).await.expect("publish_to");
    // The engine drops staging right after publishing; this must be harmless.
    staging
        .drop_graph()
        .await
        .expect("drop_graph after publish");

    let live = mk(&live_key)?;
    let node = live
        .get_node(&NodeId::new(CALLEE_ID))
        .await
        .expect("get_node on published graph")
        .expect("published graph fully queryable after staging drop");
    assert_eq!(node.name, "callee");
    let summary = live.graph_summary().await.expect("summary on published");
    assert_eq!(summary.total_nodes, 4, "published graph is complete");

    // Idempotence: dropping an absent graph succeeds.
    live.drop_graph().await.expect("drop live");
    live.drop_graph().await.expect("drop_graph is idempotent");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// bulk_load_observed fires nodes→edges in order (adapters with phase events)
/// or degrades to a plain load (trait default) — both satisfy the contract.
async fn observed_load<F>(mk: &F, ns: &str) -> anyhow::Result<()>
where
    F: Fn(&str) -> MkResult,
{
    let key = format!("{ns}_obs");
    let (artifacts, dir) = write_fixture(&key)?;
    let store = mk(&key)?;
    let _ = store.drop_graph().await;

    let obs = RecordingObserver::default();
    let stats = store
        .bulk_load_observed(&artifacts, &obs)
        .await
        .expect("bulk_load_observed");
    assert_eq!(stats.nodes, 4, "observed load stats");

    let events = obs.events.into_inner().unwrap();
    if !events.is_empty() {
        let nodes_pos = events.iter().position(|(k, _)| *k == "nodes");
        let edges_pos = events.iter().position(|(k, _)| *k == "edges");
        match (nodes_pos, edges_pos) {
            (Some(n), Some(e)) => {
                assert!(n < e, "nodes_loaded fires before edges_loaded: {events:?}");
                assert_eq!(events[n].1, 4, "nodes_loaded count matches");
                assert_eq!(events[e].1, 3, "edges_loaded count matches");
            }
            _ => panic!("adapter fired phase events but not the nodes/edges pair: {events:?}"),
        }
    }
    // Empty events = the trait's default impl (plain load) — contract satisfied.

    store.drop_graph().await.expect("drop_graph cleanup");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
