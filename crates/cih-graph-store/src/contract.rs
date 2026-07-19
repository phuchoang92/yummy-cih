//! Backend-neutral contract suite: the behaviors every `GraphStore` adapter
//! must exhibit, parameterized over a store constructor. An adapter passes by
//! running [`run_contract_suite`] against a live instance of its backend (see
//! `cih-falkor/tests/falkor_integration.rs` and
//! `cih-ladybug/tests/contract.rs`); a new backend is not considered wired in
//! until this suite is green.
//!
//! The constructor is **key-parameterized** — `mk(graph_key)` — because the
//! publish test must build a store on a staging key, publish, then construct a
//! *second* store for the destination key against the same backend instance.
//!
//! Contract violations are reported as `Err` (not panics) so the runner can
//! always clean up backend graphs and temp dirs — a failed run on a shared
//! live DB must not leak state.
//!
//! Gated behind the `test-support` feature so production builds don't carry
//! fixture code.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use cih_core::{
    Edge, EdgeKind, GraphArtifacts, GraphDelta, Node, NodeId, NodeKind, Range, VersionId,
};

use crate::{Direction, GraphStore, LoadObserver};

type MkResult = anyhow::Result<Arc<dyn GraphStore>>;

/// `anyhow::ensure!` with the condition text included, for terse call sites.
macro_rules! check {
    ($cond:expr, $($arg:tt)+) => {
        anyhow::ensure!($cond, $($arg)+)
    };
}

const CALLER_ID: &str = "Method:com.acme.Foo#caller/0";
const CALLEE_ID: &str = "Method:com.acme.Bar#callee/0";
const HANDLER_ID: &str = "Method:com.acme.Api#getThings/0";
const ROUTE_ID: &str = "Route:GET /api/things";
const CLASS_ID: &str = "Class:com.acme.Bar";
const TEST_METHOD_ID: &str = "Method:com.acme.BarTest#testCallee/0";
const CLASS_TEST_ID: &str = "Method:com.acme.BarTest#testClass/0";
const COMM_A_ID: &str = "Community:com.acme.a";
const COMM_B_ID: &str = "Community:com.acme.b";
const PROCESS_ID: &str = "Process:com.acme.OrderFlow";
const WEIRD_ID: &str = "Method:com.acme.Weird#w/0";
const ADVICE_ID: &str = "Method:com.acme.LogAspect#logCalls/1";
/// Quote + backslash + newline: proves the bulk path's cell escaping
/// round-trips (CSV COPY loaders are the usual victims).
const WEIRD_NAME: &str = "wei\"rd\\na\nme";

const CALLER_FILE: &str = "com/acme/Foo.java";
const CALLEE_FILE: &str = "com/acme/Bar.java";
const API_FILE: &str = "com/acme/Api.java";
const TEST_FILE: &str = "com/acme/BarTest.java";
const WEIRD_FILE: &str = "com/acme/Weird.java";

/// Distinct nodes in the fixture (edges list below has one deliberate
/// duplicate that adapters must collapse).
const FIXTURE_NODES: u64 = 12;
const FIXTURE_EDGES: u64 = 12;

fn node(id: &str, kind: NodeKind, name: &str, file: &str) -> Node {
    Node {
        id: NodeId::new(id),
        kind,
        name: name.to_string(),
        qualified_name: None,
        file: file.to_string(),
        range: Range::default(),
        props: None,
    }
}

fn edge(src: &str, dst: &str, kind: EdgeKind) -> Edge {
    Edge::new(
        NodeId::new(src),
        NodeId::new(dst),
        kind,
        1.0,
        "contract".to_string(),
    )
}

/// Small call graph + one route + class/test structure + communities +
/// process + similarity, sized so every read method has something to return:
///
/// ```text
/// route ←HANDLES_ROUTE─ handler ─CALLS→ caller ─CALLS→ callee (×2 in input)
/// bar_class ─HAS_METHOD→ callee;  test_method ─TESTS→ callee
/// class_test ─TESTS→ bar_class
/// caller,handler ─MEMBER_OF→ commA;  callee ─MEMBER_OF→ commB
/// caller ─STEP_IN_PROCESS→ process;  caller ─SIMILAR_TO→ callee
/// ```
fn fixture_nodes_edges() -> (Vec<Node>, Vec<Edge>) {
    let mut caller = node(CALLER_ID, NodeKind::Method, "caller", CALLER_FILE);
    caller.props = Some(serde_json::json!({
        "cyclomatic": 7, "cognitive": 9, "loopDepth": 1, "transitiveLoopDepth": 2,
    }));
    let callee = node(CALLEE_ID, NodeKind::Method, "callee", CALLEE_FILE);
    let handler = node(HANDLER_ID, NodeKind::Method, "getThings", API_FILE);
    let mut route = node(ROUTE_ID, NodeKind::Route, "GET /api/things", API_FILE);
    route.props = Some(serde_json::json!({"path": "/api/things", "httpMethod": "GET"}));
    let bar_class = node(CLASS_ID, NodeKind::Class, "Bar", CALLEE_FILE);
    let mut test_method = node(TEST_METHOD_ID, NodeKind::Method, "testCallee", TEST_FILE);
    test_method.props = Some(serde_json::json!({"stereotype": "test"}));
    let mut class_test = node(CLASS_TEST_ID, NodeKind::Method, "testClass", TEST_FILE);
    class_test.props = Some(serde_json::json!({"stereotype": "test"}));
    let mut comm_a = node(COMM_A_ID, NodeKind::Community, "commA", "");
    comm_a.props = Some(serde_json::json!({"symbolCount": 2, "cohesion": 0.5}));
    let mut comm_b = node(COMM_B_ID, NodeKind::Community, "commB", "");
    comm_b.props = Some(serde_json::json!({"symbolCount": 1, "cohesion": 0.25}));
    let process = node(PROCESS_ID, NodeKind::Process, "OrderFlow", "");
    let weird = node(WEIRD_ID, NodeKind::Method, WEIRD_NAME, WEIRD_FILE);
    let advice = node(ADVICE_ID, NodeKind::Method, "logCalls", CALLER_FILE);

    let mut similar = edge(CALLER_ID, CALLEE_ID, EdgeKind::SimilarTo);
    similar.confidence = 0.9;
    // AOP interception: reason carries the advice kind (`aop-<kind>`), the
    // shape trace_flow's `intercepted_by` annotation reads back.
    let mut advises = edge(ADVICE_ID, CALLER_ID, EdgeKind::Advises);
    advises.reason = "aop-around".to_string();

    let edges = vec![
        edge(CALLER_ID, CALLEE_ID, EdgeKind::Calls),
        edge(HANDLER_ID, CALLER_ID, EdgeKind::Calls),
        edge(HANDLER_ID, ROUTE_ID, EdgeKind::HandlesRoute),
        // Deliberate duplicate: adapters must collapse identical
        // (src, dst, kind) rows however they load.
        edge(CALLER_ID, CALLEE_ID, EdgeKind::Calls),
        edge(CLASS_ID, CALLEE_ID, EdgeKind::HasMethod),
        edge(TEST_METHOD_ID, CALLEE_ID, EdgeKind::Tests),
        edge(CLASS_TEST_ID, CLASS_ID, EdgeKind::Tests),
        edge(CALLER_ID, COMM_A_ID, EdgeKind::MemberOf),
        edge(HANDLER_ID, COMM_A_ID, EdgeKind::MemberOf),
        edge(CALLEE_ID, COMM_B_ID, EdgeKind::MemberOf),
        edge(CALLER_ID, PROCESS_ID, EdgeKind::StepInProcess),
        similar,
        advises,
    ];
    (
        vec![
            caller,
            callee,
            handler,
            route,
            bar_class,
            test_method,
            class_test,
            comm_a,
            comm_b,
            process,
            weird,
            advice,
        ],
        edges,
    )
}

fn artifacts_dir_for(key: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("{key}-artifacts"))
}

fn write_fixture(key: &str) -> anyhow::Result<GraphArtifacts> {
    let (nodes, edges) = fixture_nodes_edges();
    GraphArtifacts::write(
        &artifacts_dir_for(key),
        VersionId::new("contract-v1"),
        &nodes,
        &edges,
    )
    .context("write fixture artifacts")
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
/// instance each time. Returns `Err` on the first contract violation or
/// infrastructure failure; backend graphs and temp dirs created by the suite
/// are cleaned up either way.
pub async fn run_contract_suite<F>(mk: F) -> anyhow::Result<()>
where
    F: Fn(&str) -> MkResult,
{
    let ns = namespace();
    let mk: &dyn Fn(&str) -> MkResult = &mk;

    /// Best-effort cleanup of the case's backend graphs and temp dirs, run on
    /// success AND failure — a failed run on a shared live DB must not leak.
    async fn finish(
        mk: &dyn Fn(&str) -> MkResult,
        keys: &[String],
        name: &str,
        result: anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        for key in keys {
            if let Ok(store) = mk(key) {
                let _ = store.drop_graph().await;
            }
            let _ = std::fs::remove_dir_all(artifacts_dir_for(key));
        }
        result.with_context(|| format!("contract case '{name}'"))
    }

    let key = format!("{ns}_rt");
    let r = reads_case(mk, key.clone()).await;
    finish(mk, &[key], "reads", r).await?;

    let key = format!("{ns}_flow");
    let r = flow_case(mk, key.clone()).await;
    finish(mk, &[key], "flow", r).await?;

    let key = format!("{ns}_inc");
    let r = incremental_case(mk, key.clone()).await;
    finish(mk, &[key], "incremental", r).await?;

    let key = format!("{ns}_pub");
    let r = publish_case(mk, key.clone()).await;
    finish(mk, &[key.clone(), format!("{key}-staging")], "publish", r).await?;

    let key = format!("{ns}_obs");
    let r = observed_case(mk, key.clone()).await;
    finish(mk, &[key], "observed", r).await?;

    Ok(())
}

async fn load_fixture(
    mk: &dyn Fn(&str) -> MkResult,
    key: &str,
) -> anyhow::Result<Arc<dyn GraphStore>> {
    let artifacts = write_fixture(key)?;
    let store = mk(key)?;
    let _ = store.drop_graph().await;
    let stats = store.bulk_load(&artifacts).await.context("bulk_load")?;
    check!(
        stats.nodes == FIXTURE_NODES && stats.edges == FIXTURE_EDGES,
        "fixture load stats: got {}/{}, want {FIXTURE_NODES}/{FIXTURE_EDGES} \
         (duplicate edge must be collapsed)",
        stats.nodes,
        stats.edges
    );
    // Mirror a fresh server connect (bulk fast paths may rebuild indexes).
    store.ensure_schema().await.context("ensure_schema")?;
    Ok(store)
}

fn ids_of(nodes: &[Node]) -> Vec<&str> {
    nodes.iter().map(|n| n.id.as_str()).collect()
}

/// The bulk of the read surface over one loaded fixture.
async fn reads_case(mk: &dyn Fn(&str) -> MkResult, key: String) -> anyhow::Result<()> {
    let store = load_fixture(mk, &key).await?;
    let callee_id = NodeId::new(CALLEE_ID);

    // -- graph_summary ------------------------------------------------------
    let summary = store.graph_summary().await.context("graph_summary")?;
    check!(
        summary.total_nodes == FIXTURE_NODES && summary.total_edges == FIXTURE_EDGES,
        "summary {}/{} != {FIXTURE_NODES}/{FIXTURE_EDGES}",
        summary.total_nodes,
        summary.total_edges
    );
    let methods = summary
        .kinds
        .iter()
        .find(|k| k.kind == "Method")
        .map(|k| k.count)
        .unwrap_or(0);
    check!(methods == 7, "summary Method count {methods} != 7");

    // -- get_node (incl. escaping round-trip) -------------------------------
    let n = store.get_node(&callee_id).await?.context("callee exists")?;
    check!(n.name == "callee" && n.file == CALLEE_FILE, "callee fields");
    let weird = store
        .get_node(&NodeId::new(WEIRD_ID))
        .await?
        .context("weird-name node exists")?;
    check!(
        weird.name == WEIRD_NAME,
        "special characters must round-trip through the load path: got {:?}, want {:?}",
        weird.name,
        WEIRD_NAME
    );

    // -- neighbors: stored orientation in both query directions -------------
    let nbrs = store
        .neighbors(&callee_id, Direction::Upstream, &[EdgeKind::Calls])
        .await?;
    check!(
        nbrs.iter()
            .any(|e| e.src.as_str() == CALLER_ID && e.dst.as_str() == CALLEE_ID),
        "caller→callee visible upstream with stored orientation: {nbrs:?}"
    );
    let down = store
        .neighbors(
            &NodeId::new(CALLER_ID),
            Direction::Downstream,
            &[EdgeKind::Calls],
        )
        .await?;
    check!(
        down.iter()
            .any(|e| e.src.as_str() == CALLER_ID && e.dst.as_str() == CALLEE_ID),
        "caller→callee visible downstream with stored orientation: {down:?}"
    );

    // -- impact: depths AND parents, all three directions --------------------
    let up = store.impact(&callee_id, Direction::Upstream, 4).await?;
    let by_id = |imp: &crate::Impact, id: &str| {
        imp.affected
            .iter()
            .find(|n| n.id.as_str() == id)
            .cloned()
            .with_context(|| format!("{id} in impact"))
    };
    let caller_hit = by_id(&up, CALLER_ID)?;
    check!(caller_hit.depth == 1, "caller one hop upstream");
    check!(
        caller_hit.parent_id.as_ref().map(NodeId::as_str) == Some(CALLEE_ID),
        "1-hop parent is the root: {:?}",
        caller_hit.parent_id
    );
    let handler_hit = by_id(&up, HANDLER_ID)?;
    check!(handler_hit.depth == 2, "handler two hops upstream");
    check!(
        handler_hit.parent_id.as_ref().map(NodeId::as_str) == Some(CALLER_ID),
        "2-hop parent attribution (guards path-node ordering): {:?}",
        handler_hit.parent_id
    );
    let downstream = store
        .impact(&NodeId::new(HANDLER_ID), Direction::Downstream, 4)
        .await?;
    let callee_hit = by_id(&downstream, CALLEE_ID)?;
    check!(
        callee_hit.depth == 2,
        "callee two hops downstream of handler"
    );
    check!(
        callee_hit.parent_id.as_ref().map(NodeId::as_str) == Some(CALLER_ID),
        "downstream 2-hop parent: {:?}",
        callee_hit.parent_id
    );
    let both = store
        .impact(&NodeId::new(CALLER_ID), Direction::Both, 2)
        .await?;
    check!(
        by_id(&both, CALLEE_ID)?.depth == 1 && by_id(&both, HANDLER_ID)?.depth == 1,
        "Both direction reaches callee and handler at depth 1"
    );

    // -- call_chain ---------------------------------------------------------
    let chains = store
        .call_chain(&NodeId::new(HANDLER_ID), &callee_id, 5)
        .await?;
    check!(
        chains
            .iter()
            .any(|p| { ids_of_path(p) == [HANDLER_ID, CALLER_ID, CALLEE_ID] }),
        "handler→caller→callee chain found: {chains:?}"
    );

    // -- context ------------------------------------------------------------
    let ctx = store.context(&NodeId::new(CALLER_ID)).await?;
    check!(
        ctx.callers.iter().any(|n| n.id.as_str() == HANDLER_ID),
        "context callers include handler"
    );
    check!(
        ctx.callees.iter().any(|n| n.id.as_str() == CALLEE_ID),
        "context callees include callee"
    );
    check!(
        ctx.processes.iter().any(|p| p == PROCESS_ID),
        "context processes include the process: {:?}",
        ctx.processes
    );
    check!(
        ctx.community.as_ref().map(|c| c.id.as_str()) == Some(COMM_A_ID),
        "context community is commA: {:?}",
        ctx.community
    );

    // -- routes -------------------------------------------------------------
    let routes = store.route_map(None, 50).await?;
    let r = routes
        .iter()
        .find(|r| r.path == "/api/things")
        .context("route present in route_map")?;
    check!(
        r.http_method == "GET" && r.handler_id.as_str() == HANDLER_ID,
        "route fields: {r:?}"
    );
    let filtered = store.route_map(Some("/nope"), 50).await?;
    check!(filtered.is_empty(), "prefix filter excludes: {filtered:?}");

    // -- name/file lookups --------------------------------------------------
    let cands = store.candidates_by_name("callee", 10).await?;
    check!(
        cands.iter().any(|n| n.id.as_str() == CALLEE_ID),
        "callee found by short name"
    );
    let in_files = store.nodes_in_files(&[CALLER_FILE.to_string()]).await?;
    check!(
        in_files.iter().any(|n| n.id.as_str() == CALLER_ID),
        "caller found via nodes_in_files"
    );

    // -- processes / communities --------------------------------------------
    let procs = store
        .processes_for_symbols(&[NodeId::new(CALLER_ID), NodeId::new(CALLEE_ID)])
        .await?;
    check!(
        procs == vec![PROCESS_ID.to_string()],
        "processes_for_symbols: {procs:?}"
    );
    let comms = store.communities().await?;
    check!(
        comms.len() == 2 && comms[0].id == COMM_A_ID && comms[0].symbol_count == 2,
        "communities ordered by size: {comms:?}"
    );
    let sym_comms = store
        .symbol_communities(&[NodeId::new(CALLER_ID), NodeId::new(CALLEE_ID)])
        .await?;
    let find_comm = |id: &str| {
        sym_comms
            .iter()
            .find(|(nid, _)| nid.as_str() == id)
            .map(|(_, c)| c.id.as_str())
    };
    check!(
        find_comm(CALLER_ID) == Some(COMM_A_ID) && find_comm(CALLEE_ID) == Some(COMM_B_ID),
        "symbol_communities: {sym_comms:?}"
    );
    let cg = store.community_graph().await?;
    check!(
        cg.iter()
            .any(|e| e.src == COMM_A_ID && e.dst == COMM_B_ID && e.weight >= 1),
        "commA→commB cross-community CALLS edge: {cg:?}"
    );

    // -- tests --------------------------------------------------------------
    let cov = store.test_coverage(&callee_id).await?;
    let cov_ids = ids_of(&cov);
    check!(
        cov_ids.contains(&TEST_METHOD_ID),
        "direct TESTS edge found: {cov_ids:?}"
    );
    check!(
        cov_ids.contains(&CLASS_TEST_ID),
        "owner-class TESTS found via correlated subquery: {cov_ids:?}"
    );
    let tff = store.tests_for_files(&[CALLEE_FILE.to_string()]).await?;
    let tff_ids = ids_of(&tff);
    check!(
        tff_ids.contains(&TEST_METHOD_ID) && tff_ids.contains(&CLASS_TEST_ID),
        "tests_for_files finds both tests: {tff_ids:?}"
    );
    let untested = store.untested_symbols("com/acme", 100).await?;
    let untested_ids: HashSet<&str> = untested.iter().map(|n| n.id.as_str()).collect();
    check!(
        untested_ids.contains(CALLER_ID)
            && untested_ids.contains(HANDLER_ID)
            && untested_ids.contains(WEIRD_ID),
        "untested symbols include the untested methods (a missing stereotype \
         must not exclude a node): {untested_ids:?}"
    );
    check!(
        !untested_ids.contains(CALLEE_ID)
            && !untested_ids.contains(CLASS_ID)
            && !untested_ids.contains(TEST_METHOD_ID),
        "tested/test symbols excluded: {untested_ids:?}"
    );

    // -- similarity / complexity --------------------------------------------
    let sim = store
        .similar_methods(&NodeId::new(CALLER_ID), 0.5, 10)
        .await?;
    check!(
        sim.iter()
            .any(|s| s.id.as_str() == CALLEE_ID && (s.jaccard - 0.9).abs() < 1e-6),
        "similar_methods returns SIMILAR_TO with confidence: {sim:?}"
    );
    let hot = store
        .complexity_hotspots(Some(5), Some(5), Some(1), 10)
        .await?;
    check!(
        hot.iter().any(|h| h.id.as_str() == CALLER_ID
            && h.cyclomatic == 7
            && h.transitive_loop_depth == 2),
        "complexity hotspot found with promoted metrics: {hot:?}"
    );

    // -- subgraph / overview -------------------------------------------------
    let sub = store.subgraph(&[NodeId::new(CALLER_ID)], 1).await?;
    let sub_ids: HashSet<&str> = sub.nodes.iter().map(|n| n.id.as_str()).collect();
    check!(
        sub_ids.contains(CALLEE_ID) && sub_ids.contains(HANDLER_ID) && sub_ids.contains(COMM_A_ID),
        "subgraph radius-1 members: {sub_ids:?}"
    );
    check!(!sub.edges.is_empty(), "subgraph carries edges");

    let mv = store
        .graph_overview(50, 100, Some(&["Method".to_string()]))
        .await?;
    check!(
        mv.nodes.len() == 7 && mv.total_nodes == FIXTURE_NODES,
        "kind-filtered overview: {} nodes, total {}",
        mv.nodes.len(),
        mv.total_nodes
    );
    // Edges are listed only among SELECTED nodes; the Method selection has
    // caller→callee CALLS (the default structural selection below has no
    // intra-set edges in this fixture, so it asserts totals only).
    check!(
        mv.edges.iter().any(|e| {
            e.source.as_str() == CALLER_ID
                && e.target.as_str() == CALLEE_ID
                && e.kind == EdgeKind::Calls
        }),
        "kind-filtered overview lists intra-selection edges: {:?}",
        mv.edges
    );
    let ov = store.graph_overview(50, 100, None).await?;
    let ov_ids: HashSet<&str> = ov.nodes.iter().map(|n| n.node.id.as_str()).collect();
    check!(
        ov_ids.contains(ROUTE_ID) && ov_ids.contains(COMM_A_ID) && ov_ids.contains(CLASS_ID),
        "default overview: structural pass then class-family pass: {ov_ids:?}"
    );
    check!(
        ov.total_edges == FIXTURE_EDGES,
        "overview totals: {} total edges",
        ov.total_edges
    );

    store.drop_graph().await.context("drop_graph cleanup")?;
    Ok(())
}

fn ids_of_path(p: &crate::Path) -> Vec<&str> {
    p.nodes.iter().map(NodeId::as_str).collect()
}

/// flow_downstream from a Route entry: route→handler via HANDLES_ROUTE, then
/// the CALLS chain, with depths, parents, and via kinds. (On dialects with
/// recursive-path features this exercises multi-relationship-type recursion.)
async fn flow_case(mk: &dyn Fn(&str) -> MkResult, key: String) -> anyhow::Result<()> {
    let store = load_fixture(mk, &key).await?;
    let hops = store
        .flow_downstream(&NodeId::new(ROUTE_ID), 6)
        .await
        .context("flow_downstream")?;
    let find = |id: &str| {
        hops.iter()
            .find(|h| h.node.id.as_str() == id)
            .with_context(|| format!("{id} in flow: {hops:?}"))
    };
    let route = find(ROUTE_ID)?;
    check!(
        route.node.depth == 0 && route.via.is_none(),
        "route is the root hop"
    );
    let handler = find(HANDLER_ID)?;
    check!(
        handler.node.depth == 1
            && handler.via.as_ref().map(|v| v.kind.as_str()) == Some("HANDLES_ROUTE"),
        "handler at depth 1 via HANDLES_ROUTE: {handler:?}"
    );
    let caller = find(CALLER_ID)?;
    check!(
        caller.node.depth == 2
            && caller.node.parent_id.as_ref().map(NodeId::as_str) == Some(HANDLER_ID)
            && caller.via.as_ref().map(|v| v.kind.as_str()) == Some("CALLS"),
        "caller at depth 2 from handler via CALLS: {caller:?}"
    );
    // AOP: the ADVISES edge into `caller` must surface as an intercepted_by
    // annotation (advice id + kind), not as an extra path hop.
    check!(
        caller.node.intercepted_by.len() == 1
            && caller.node.intercepted_by[0].advice.as_str() == ADVICE_ID
            && caller.node.intercepted_by[0].advice_kind == "around",
        "caller intercepted_by around-advice: {:?}",
        caller.node.intercepted_by
    );
    check!(
        !hops.iter().any(|h| h.node.id.as_str() == ADVICE_ID),
        "the advice method is not a flow hop"
    );
    let callee = find(CALLEE_ID)?;
    check!(
        callee.node.depth == 3
            && callee.node.parent_id.as_ref().map(NodeId::as_str) == Some(CALLER_ID),
        "callee at depth 3 from caller: {callee:?}"
    );
    store.drop_graph().await.context("drop_graph cleanup")?;
    Ok(())
}

/// upsert_incremental: changed-file delete + reload semantics.
async fn incremental_case(mk: &dyn Fn(&str) -> MkResult, key: String) -> anyhow::Result<()> {
    let store = load_fixture(mk, &key).await?;

    // Foo.java changed: `caller` was renamed to `caller2` (new id).
    let new_caller = node(
        "Method:com.acme.Foo#caller2/0",
        NodeKind::Method,
        "caller2",
        CALLER_FILE,
    );
    let delta = GraphDelta {
        changed_files: vec![CALLER_FILE.to_string()],
        removed_files: vec![],
        nodes: vec![new_caller.clone()],
        edges: vec![edge(new_caller.id.as_str(), CALLEE_ID, EdgeKind::Calls)],
    };
    store
        .upsert_incremental(&delta)
        .await
        .context("upsert_incremental")?;

    check!(
        store.get_node(&NodeId::new(CALLER_ID)).await?.is_none(),
        "old node from the changed file was deleted"
    );
    check!(
        store.get_node(&new_caller.id).await?.is_some(),
        "replacement node was loaded"
    );
    check!(
        store.get_node(&NodeId::new(CALLEE_ID)).await?.is_some(),
        "node in an untouched file survived the delta"
    );

    store.drop_graph().await.context("drop_graph cleanup")?;
    Ok(())
}

/// publish_to + drop_graph: the staging→live swap. Encodes the port guarantee:
/// after `publish_to(dest)` returns, dropping the staging graph must not affect
/// the published data (the engine does exactly this after every load).
async fn publish_case(mk: &dyn Fn(&str) -> MkResult, live_key: String) -> anyhow::Result<()> {
    let staging_key = format!("{live_key}-staging");
    let artifacts = write_fixture(&live_key)?;

    let staging = mk(&staging_key)?;
    let _ = staging.drop_graph().await;
    staging
        .bulk_load(&artifacts)
        .await
        .context("bulk_load staging")?;
    staging.publish_to(&live_key).await.context("publish_to")?;
    // The engine drops staging right after publishing; this must be harmless.
    staging
        .drop_graph()
        .await
        .context("drop_graph after publish")?;

    let live = mk(&live_key)?;
    let n = live
        .get_node(&NodeId::new(CALLEE_ID))
        .await?
        .context("published graph fully queryable after staging drop")?;
    check!(n.name == "callee", "published node intact");
    let summary = live.graph_summary().await?;
    check!(
        summary.total_nodes == FIXTURE_NODES,
        "published graph is complete: {}",
        summary.total_nodes
    );

    // Idempotence: dropping an absent graph succeeds.
    live.drop_graph().await.context("drop live")?;
    live.drop_graph()
        .await
        .context("drop_graph is idempotent")?;
    Ok(())
}

/// bulk_load_observed fires nodes→edges in order (adapters with phase events)
/// or degrades to a plain load (trait default) — both satisfy the contract.
async fn observed_case(mk: &dyn Fn(&str) -> MkResult, key: String) -> anyhow::Result<()> {
    let artifacts = write_fixture(&key)?;
    let store = mk(&key)?;
    let _ = store.drop_graph().await;

    let obs = RecordingObserver::default();
    let stats = store
        .bulk_load_observed(&artifacts, &obs)
        .await
        .context("bulk_load_observed")?;
    check!(
        stats.nodes == FIXTURE_NODES,
        "observed load stats: {}",
        stats.nodes
    );

    let events = obs.events.into_inner().unwrap();
    if !events.is_empty() {
        let nodes_pos = events.iter().position(|(k, _)| *k == "nodes");
        let edges_pos = events.iter().position(|(k, _)| *k == "edges");
        match (nodes_pos, edges_pos) {
            (Some(n), Some(e)) => {
                check!(n < e, "nodes_loaded fires before edges_loaded: {events:?}");
                check!(
                    events[n].1 == FIXTURE_NODES && events[e].1 == FIXTURE_EDGES,
                    "event counts match stats: {events:?}"
                );
            }
            _ => {
                anyhow::bail!("adapter fired phase events but not the nodes/edges pair: {events:?}")
            }
        }
    }
    // Empty events = the trait's default impl (plain load) — contract satisfied.

    store.drop_graph().await.context("drop_graph cleanup")?;
    Ok(())
}
