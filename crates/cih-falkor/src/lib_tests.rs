use super::{FalkorStore, GraphStoreError};
use crate::serialize::cstr;
use std::time::Duration;

#[test]
fn cstr_escapes_backslash_and_single_quote() {
    assert_eq!(cstr("a\\b's"), "'a\\\\b\\'s'");
    assert_eq!(cstr("line\nnext\tcell\rend"), "'line\\nnext\\tcell\\rend'");
}

// Backpressure is pure semaphore logic — no FalkorDB needed. `Client::open`
// only parses the URL, it does not dial, so this stays hermetic.
#[tokio::test]
async fn query_limit_sheds_when_saturated() {
    let store = FalkorStore::connect("redis://127.0.0.1:6379", "test")
        .expect("connect parses url")
        .with_query_limit(1, Duration::from_millis(50));

    // Hold the only permit for the duration of the test.
    let _held = store
        .acquire_permit()
        .await
        .expect("first acquire succeeds");

    // The next acquire can't get a slot and sheds after the timeout.
    let err = store
        .acquire_permit()
        .await
        .expect_err("second acquire sheds");
    match err {
        GraphStoreError::Backend(msg) => assert!(
            msg.contains("overloaded"),
            "expected overloaded error, got: {msg}"
        ),
        other => panic!("expected Backend overloaded error, got: {other:?}"),
    }
}

// With slack in the limit, concurrent acquires all succeed (no false shedding).
#[tokio::test]
async fn query_limit_allows_within_capacity() {
    let store = FalkorStore::connect("redis://127.0.0.1:6379", "test")
        .expect("connect parses url")
        .with_query_limit(2, Duration::from_millis(50));

    let a = store.acquire_permit().await.expect("first slot");
    let b = store.acquire_permit().await.expect("second slot");
    drop((a, b));
}

#[test]
fn is_loading_error_detects_busy_loading() {
    // The exact message FalkorDB/redis surfaces during dataset load, as seen in
    // the field log — mapped to GraphStoreError::Backend via run()'s e.to_string().
    let loading = GraphStoreError::Backend(
        "graph backend error: An error was signalled by the server - BusyLoadingError: \
         Redis is loading the dataset in memory"
            .into(),
    );
    assert!(FalkorStore::is_loading_error(&loading));

    // Non-loading backend errors must NOT be treated as loading (fail fast).
    for other in [
        "graph backend error: syntax error",
        "graph store overloaded: concurrent query limit reached",
        "connection refused",
    ] {
        assert!(
            !FalkorStore::is_loading_error(&GraphStoreError::Backend(other.into())),
            "false positive on: {other}"
        );
    }
    // Wrong variant is never a loading error.
    assert!(!FalkorStore::is_loading_error(&GraphStoreError::NotFound(
        "x".into()
    )));
}

#[test]
fn load_wait_budget_defaults_to_600s() {
    // Only asserts the default when the env var is unset (test env doesn't set it).
    if std::env::var("CIH_FALKOR_LOAD_WAIT_SECS").is_err() {
        assert_eq!(FalkorStore::load_wait_budget(), Duration::from_secs(600));
    }
}

// ── Route-entry flow assembly (pure — no FalkorDB) ───────────────────────────
//
// `assemble_route_flow` is the language-agnostic half of the Route-entry
// `flow_downstream` fix: given a route id and its handlers (each already paired
// with its own downstream walk), it prepends the reversed `HANDLES_ROUTE` hop.
// It touches no I/O, so these stay hermetic.
use crate::query::assemble_route_flow;
use cih_core::{NodeId, NodeKind};
use cih_graph_store::{FlowEdge, FlowHop, FlowNode};

fn fnode(id: &str, name: &str, depth: u32, parent: Option<&str>) -> FlowNode {
    FlowNode {
        id: NodeId::new(id.to_string()),
        kind: NodeKind::Function,
        name: name.to_string(),
        qualified_name: Some(id.to_string()),
        file: "f.ts".to_string(),
        depth,
        parent_id: parent.map(|p| NodeId::new(p.to_string())),
    }
}

fn root_hop(node: FlowNode) -> FlowHop {
    FlowHop { node, via: None }
}

fn calls_hop(node: FlowNode) -> FlowHop {
    FlowHop {
        node,
        via: Some(FlowEdge {
            kind: "CALLS".to_string(),
            call_sites: Vec::new(),
        }),
    }
}

#[test]
fn assemble_route_flow_prepends_handler_via_handles_route() {
    let route = NodeId::new("Route:graphql:MUTATION:signup".to_string());
    let handler = fnode("Function:AuthResolver#signup/1", "signup", 1, None);
    // The downstream walk FROM the handler: its own root hop at depth 0, then
    // its callees (as `flow_downstream` would return them for a method entry).
    let sub = vec![
        root_hop(fnode("Function:AuthResolver#signup/1", "signup", 0, None)),
        calls_hop(fnode(
            "Function:AuthService#createUser/1",
            "createUser",
            1,
            Some("Function:AuthResolver#signup/1"),
        )),
        calls_hop(fnode(
            "Function:AuthService#hashPassword/1",
            "hashPassword",
            2,
            Some("Function:AuthService#createUser/1"),
        )),
    ];

    let hops = assemble_route_flow(&route, vec![(handler, sub)]);

    // Root is the route itself: depth 0, no via, kind Route, "Route:" stripped.
    assert_eq!(hops[0].node.id.as_str(), "Route:graphql:MUTATION:signup");
    assert_eq!(hops[0].node.kind, NodeKind::Route);
    assert_eq!(hops[0].node.depth, 0);
    assert_eq!(hops[0].node.name, "graphql:MUTATION:signup");
    assert!(hops[0].via.is_none());

    // Handler reached via the reversed HANDLES_ROUTE at depth 1, parented on
    // the route.
    assert_eq!(hops[1].node.id.as_str(), "Function:AuthResolver#signup/1");
    assert_eq!(hops[1].node.depth, 1);
    assert_eq!(hops[1].via.as_ref().unwrap().kind, "HANDLES_ROUTE");
    assert_eq!(
        hops[1].node.parent_id.as_ref().unwrap().as_str(),
        "Route:graphql:MUTATION:signup"
    );

    // Downstream shifted one level past the route; the handler's own root hop
    // (sub[0]) is dropped, not duplicated.
    assert_eq!(hops[2].node.name, "createUser");
    assert_eq!(hops[2].node.depth, 2);
    assert_eq!(hops[2].via.as_ref().unwrap().kind, "CALLS");
    assert_eq!(hops[3].node.name, "hashPassword");
    assert_eq!(hops[3].node.depth, 3);

    // route + handler + 2 callees, with no duplicated handler root.
    assert_eq!(hops.len(), 4);
}

#[test]
fn assemble_route_flow_dedups_nodes_shared_across_handlers() {
    let route = NodeId::new("Route:GET /x".to_string());
    let h1 = fnode("Function:H1", "h1", 1, None);
    let sub1 = vec![
        root_hop(fnode("Function:H1", "h1", 0, None)),
        calls_hop(fnode("Function:Shared", "shared", 1, Some("Function:H1"))),
    ];
    let h2 = fnode("Function:H2", "h2", 1, None);
    let sub2 = vec![
        root_hop(fnode("Function:H2", "h2", 0, None)),
        calls_hop(fnode("Function:Shared", "shared", 1, Some("Function:H2"))),
    ];

    let hops = assemble_route_flow(&route, vec![(h1, sub1), (h2, sub2)]);

    // A downstream node reachable from both handlers is emitted once (first
    // occurrence wins), and no handler root leaks in as a duplicate.
    let shared = hops
        .iter()
        .filter(|h| h.node.id.as_str() == "Function:Shared")
        .count();
    assert_eq!(shared, 1);
    let ids: Vec<&str> = hops.iter().map(|h| h.node.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "Route:GET /x",
            "Function:H1",
            "Function:Shared",
            "Function:H2"
        ]
    );
}
