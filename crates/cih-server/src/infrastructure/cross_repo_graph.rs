//! Cross-repo flow tracing over per-repo graph artifacts (the `shape_check`
//! pattern — no multi-graph server, no merged graph). A pure BFS core walks
//! one repo's `ArtifactGraph`, hops to sibling repos through the group's
//! precomputed `ContractMatch` rows, and carries explicit budgets. Entirely
//! artifacts-based, including the first leg (uniform semantics; hermetic
//! tests); the accepted trade-off is no Falkor `callSites` enrichment.
//!
//! Graph views share the process-wide `ArtifactSnapshot` also used by taint and
//! shape checking. Snapshot freshness tracks both graph files; positional
//! adjacency indexes are initialized lazily inside the heavy blocking lane.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use cih_core::{ContractMatch, ContractMatchKind, Edge, EdgeKind, Node};
use serde::Serialize;

use crate::domain::error::AppError;
use crate::domain::repository::ResolvedRepo;
use crate::infrastructure::artifact_repository::{ArtifactRepository, ArtifactSnapshot};
use crate::utils::node_prop_str_owned;

// ── Artifact graph ───────────────────────────────────────────────────────────

pub(crate) struct ArtifactGraph {
    snapshot: Arc<ArtifactSnapshot>,
    indexes: Arc<crate::infrastructure::artifact_repository::ArtifactIndexes>,
}

impl ArtifactGraph {
    #[cfg(test)]
    pub(crate) fn build(
        nodes: Vec<Node>,
        edges: Vec<Edge>,
        _nodes_mtime: Option<std::time::SystemTime>,
        _edges_mtime: Option<std::time::SystemTime>,
    ) -> Self {
        Self::from_snapshot(Arc::new(ArtifactSnapshot::from_memory(nodes, edges)))
    }

    fn from_snapshot(snapshot: Arc<ArtifactSnapshot>) -> Self {
        let indexes = snapshot.indexes().clone();
        Self { snapshot, indexes }
    }

    pub(crate) fn node(&self, id: &str) -> Option<&Node> {
        self.indexes
            .node_by_id
            .get(id)
            .map(|index| &self.snapshot.nodes[*index])
    }

    pub(crate) fn contains_node(&self, id: &str) -> bool {
        self.indexes.node_by_id.contains_key(id)
    }

    pub(crate) fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.snapshot.nodes.iter()
    }

    pub(crate) fn out<'a>(&'a self, id: &str) -> impl Iterator<Item = &'a Edge> {
        self.indexes
            .outgoing_edges
            .get(id)
            .into_iter()
            .flatten()
            .map(|index| &self.snapshot.edges[*index])
    }

    pub(crate) fn incoming<'a>(&'a self, id: &str) -> impl Iterator<Item = &'a Edge> {
        self.indexes
            .incoming_edges
            .get(id)
            .into_iter()
            .flatten()
            .map(|index| &self.snapshot.edges[*index])
    }

    #[cfg(test)]
    fn snapshot(&self) -> &Arc<ArtifactSnapshot> {
        &self.snapshot
    }
}

/// Lightweight graph views over the process-wide shared artifact cache.
#[derive(Clone)]
pub(crate) struct XflowState {
    artifacts: Arc<dyn ArtifactRepository>,
}

impl XflowState {
    pub(crate) fn new(artifacts: Arc<dyn ArtifactRepository>) -> Self {
        Self { artifacts }
    }

    pub(crate) async fn graph_for(
        &self,
        repo: &ResolvedRepo,
    ) -> Result<Arc<ArtifactGraph>, AppError> {
        self.artifacts
            .indexed_snapshot(repo)
            .await
            .map(ArtifactGraph::from_snapshot)
            .map(Arc::new)
    }
}

// ── Trace core ───────────────────────────────────────────────────────────────

/// Edge kinds the downstream walk follows (mirrors `flow_downstream`'s set).
const FLOW_EDGE_KINDS: [EdgeKind; 5] = [
    EdgeKind::Calls,
    EdgeKind::HandlesRoute,
    EdgeKind::ExternalCall,
    EdgeKind::PublishesEvent,
    EdgeKind::ListensTo,
];

pub(crate) const DEFAULT_DEPTH: u32 = 6;
pub(crate) const MAX_DEPTH: u32 = 10;
pub(crate) const DEFAULT_HOPS: u32 = 3;
pub(crate) const NODE_CAP: usize = 200;

#[derive(Debug, Serialize)]
pub(crate) struct XStep {
    pub repo: String,
    pub from: String,
    pub to: String,
    pub via: XVia,
    /// Depth within the current repo leg (resets after a contract hop).
    pub depth: u32,
    /// Number of cross-repo contract hops taken to reach this step.
    pub hop: u32,
}

#[derive(Debug, Serialize)]
pub(crate) struct XVia {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_key: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Truncation {
    pub repo: String,
    pub at_node: String,
    pub reason: String,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct XTrace {
    pub steps: Vec<XStep>,
    pub truncated: Vec<Truncation>,
}

struct WorkItem {
    repo: String,
    node: String,
    hop: u32,
}

/// BFS across repos. `graphs` returns the artifact graph for a repo name, or
/// `None` when the repo/artifacts are unavailable — that hop is recorded as a
/// truncation, never a hard failure.
pub(crate) fn trace_across(
    graphs: &mut dyn FnMut(&str) -> Option<Arc<ArtifactGraph>>,
    contracts: &[ContractMatch],
    start_repo: &str,
    entry_id: &str,
    max_depth: u32,
    max_hops: u32,
) -> XTrace {
    let mut trace = XTrace::default();
    let mut visited: HashSet<(String, String)> = HashSet::new();
    let mut queue: VecDeque<WorkItem> = VecDeque::new();
    queue.push_back(WorkItem {
        repo: start_repo.to_string(),
        node: entry_id.to_string(),
        hop: 0,
    });

    while let Some(item) = queue.pop_front() {
        if trace.steps.len() >= NODE_CAP {
            trace.truncated.push(Truncation {
                repo: item.repo,
                at_node: item.node,
                reason: format!("node cap {NODE_CAP} reached"),
            });
            break;
        }
        if !visited.insert((item.repo.clone(), item.node.clone())) {
            continue;
        }
        let Some(graph) = graphs(&item.repo) else {
            trace.truncated.push(Truncation {
                repo: item.repo.clone(),
                at_node: item.node.clone(),
                reason: "artifacts unavailable — re-run analyze on this repo".into(),
            });
            continue;
        };
        walk_repo_leg(
            &graph,
            contracts,
            &item,
            max_depth,
            max_hops,
            &mut visited,
            &mut queue,
            &mut trace,
        );
    }
    trace
}

#[allow(clippy::too_many_arguments)] // BFS plumbing shared between legs
fn walk_repo_leg(
    graph: &ArtifactGraph,
    contracts: &[ContractMatch],
    item: &WorkItem,
    max_depth: u32,
    max_hops: u32,
    visited: &mut HashSet<(String, String)>,
    queue: &mut VecDeque<WorkItem>,
    trace: &mut XTrace,
) {
    let mut leg: VecDeque<(String, u32)> = VecDeque::new();
    leg.push_back((item.node.clone(), 0));
    let mut leg_seen: HashSet<String> = HashSet::new();
    leg_seen.insert(item.node.clone());

    while let Some((node_id, depth)) = leg.pop_front() {
        if trace.steps.len() >= NODE_CAP {
            trace.truncated.push(Truncation {
                repo: item.repo.clone(),
                at_node: node_id,
                reason: format!("node cap {NODE_CAP} reached"),
            });
            return;
        }

        // Cross-repo hops out of terminal contract nodes; Route nodes (the
        // landing point of an HTTP hop) continue through their handlers via
        // the inverse HandlesRoute — a route has no outgoing flow edges.
        if let Some(node) = graph.node(&node_id) {
            match node.kind {
                cih_core::NodeKind::ExternalEndpoint => {
                    hop_http(
                        contracts,
                        &item.repo,
                        node_id.as_str(),
                        item.hop,
                        max_hops,
                        queue,
                        trace,
                    );
                    continue;
                }
                cih_core::NodeKind::KafkaTopic => {
                    let topic = node_prop_str_owned(node, "topic").unwrap_or(node.name.clone());
                    hop_event(
                        contracts,
                        &item.repo,
                        node_id.as_str(),
                        &topic,
                        item.hop,
                        max_hops,
                        queue,
                        trace,
                    );
                    continue;
                }
                cih_core::NodeKind::Route => {
                    for handler in route_handlers(graph, &node_id) {
                        if !leg_seen.insert(handler.clone()) {
                            continue;
                        }
                        visited.insert((item.repo.clone(), handler.clone()));
                        trace.steps.push(XStep {
                            repo: item.repo.clone(),
                            from: node_id.clone(),
                            to: handler.clone(),
                            via: XVia {
                                kind: "HANDLES_ROUTE".to_string(),
                                match_key: None,
                            },
                            depth,
                            hop: item.hop,
                        });
                        leg.push_back((handler, depth));
                    }
                    continue;
                }
                _ => {}
            }
        }

        if depth >= max_depth {
            continue;
        }

        for edge in graph.out(&node_id) {
            if !FLOW_EDGE_KINDS.contains(&edge.kind) {
                continue;
            }
            let dst = edge.dst.as_str().to_string();
            if !leg_seen.insert(dst.clone()) {
                continue;
            }
            visited.insert((item.repo.clone(), dst.clone()));
            trace.steps.push(XStep {
                repo: item.repo.clone(),
                from: node_id.clone(),
                to: dst.clone(),
                via: XVia {
                    kind: edge.kind.cypher_label().to_string(),
                    match_key: None,
                },
                depth: depth + 1,
                hop: item.hop,
            });
            leg.push_back((dst, depth + 1));
        }
    }
}

/// HTTP hop: `ExternalEndpoint` in repo R → HttpRoute rows consumed by R →
/// provider repo's Route node → (in the provider leg) inverse `HandlesRoute`
/// resolves the handler and the walk continues downstream from it.
#[allow(clippy::too_many_arguments)]
fn hop_http(
    contracts: &[ContractMatch],
    repo: &str,
    endpoint_id: &str,
    hop: u32,
    max_hops: u32,
    queue: &mut VecDeque<WorkItem>,
    trace: &mut XTrace,
) {
    for row in contracts {
        if row.kind != ContractMatchKind::HttpRoute
            || row.consumer_repo != repo
            || row.consumer_id != endpoint_id
        {
            continue;
        }
        if hop >= max_hops {
            trace.truncated.push(Truncation {
                repo: repo.to_string(),
                at_node: endpoint_id.to_string(),
                reason: format!("max_hops {max_hops} reached"),
            });
            return;
        }
        trace.steps.push(XStep {
            repo: row.provider_repo.clone(),
            from: endpoint_id.to_string(),
            to: row.provider_id.clone(),
            via: XVia {
                kind: "CONTRACT".to_string(),
                match_key: Some(row.match_key.clone()),
            },
            depth: 0,
            hop: hop + 1,
        });
        queue.push_back(WorkItem {
            repo: row.provider_repo.clone(),
            node: row.provider_id.clone(),
            hop: hop + 1,
        });
    }
}

/// Event hop: `KafkaTopic` in repo R (reached from a publisher) → event rows
/// published by R for this topic → listener callables in consumer repos.
#[allow(clippy::too_many_arguments)]
fn hop_event(
    contracts: &[ContractMatch],
    repo: &str,
    topic_id: &str,
    topic: &str,
    hop: u32,
    max_hops: u32,
    queue: &mut VecDeque<WorkItem>,
    trace: &mut XTrace,
) {
    for row in contracts {
        if row.kind == ContractMatchKind::HttpRoute
            || row.provider_repo != repo
            || row.match_key != topic
        {
            continue;
        }
        if hop >= max_hops {
            trace.truncated.push(Truncation {
                repo: repo.to_string(),
                at_node: topic_id.to_string(),
                reason: format!("max_hops {max_hops} reached"),
            });
            return;
        }
        trace.steps.push(XStep {
            repo: row.consumer_repo.clone(),
            from: topic_id.to_string(),
            to: row.consumer_id.clone(),
            via: XVia {
                kind: "CONTRACT".to_string(),
                match_key: Some(row.match_key.clone()),
            },
            depth: 0,
            hop: hop + 1,
        });
        queue.push_back(WorkItem {
            repo: row.consumer_repo.clone(),
            node: row.consumer_id.clone(),
            hop: hop + 1,
        });
    }
}

/// A Route node has no outgoing flow edges — the walk continues from its
/// handler, found via the **inverse** `HandlesRoute` (handler → route). Called
/// at the start of a provider leg; regular legs pass through unaffected.
pub(crate) fn route_handlers(graph: &ArtifactGraph, route_id: &str) -> Vec<String> {
    graph
        .incoming(route_id)
        .filter(|edge| edge.kind == EdgeKind::HandlesRoute)
        .map(|edge| edge.src.as_str().to_string())
        .collect()
}

/// Resolve an entry point inside one repo's artifacts: exact node id first,
/// then unique name / qualified-name match. `Err` carries candidate ids when
/// ambiguous, or an empty vec when not found.
pub(crate) fn resolve_entry(graph: &ArtifactGraph, entry: &str) -> Result<String, Vec<String>> {
    if graph.contains_node(entry) {
        return Ok(entry.to_string());
    }
    let matches: Vec<&str> = graph
        .nodes()
        .filter(|node| node.name == entry || node.qualified_name.as_deref() == Some(entry))
        .map(|node| node.id.as_str())
        .collect();
    match matches.as_slice() {
        [only] => Ok(only.to_string()),
        _ => Err(matches.iter().map(|id| id.to_string()).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::artifact_repository::{ArtifactCache, ArtifactRepository};
    use cih_core::{NodeId, NodeKind, Range};
    use std::collections::HashMap;

    fn node(id: &str, kind: NodeKind) -> Node {
        Node {
            id: NodeId::new(id),
            kind,
            name: id.rsplit(':').next().unwrap_or(id).to_string(),
            qualified_name: None,
            file: "f".into(),
            range: Range::default(),
            props: None,
        }
    }

    fn topic_node(id: &str, topic: &str) -> Node {
        let mut n = node(id, NodeKind::KafkaTopic);
        n.props = Some(serde_json::json!({ "topic": topic }));
        n
    }

    fn edge(src: &str, dst: &str, kind: EdgeKind) -> Edge {
        Edge::new(NodeId::new(src), NodeId::new(dst), kind, 1.0, "t".into())
    }

    fn http_row(
        provider_repo: &str,
        provider_id: &str,
        consumer_repo: &str,
        consumer_id: &str,
    ) -> ContractMatch {
        ContractMatch {
            kind: ContractMatchKind::HttpRoute,
            provider_repo: provider_repo.into(),
            provider_id: provider_id.into(),
            consumer_repo: consumer_repo.into(),
            consumer_id: consumer_id.into(),
            match_key: "GET /api/orders/{*}".into(),
        }
    }

    /// checkout: m1 -CALLS-> m2 -EXTERNAL_CALL-> endpoint
    /// orders:   h1 -HANDLES_ROUTE-> route; h1 -CALLS-> h2
    fn two_repo_fixture() -> (HashMap<String, Arc<ArtifactGraph>>, Vec<ContractMatch>) {
        let checkout = ArtifactGraph::build(
            vec![
                node("Method:co.C#m1/0", NodeKind::Method),
                node("Method:co.C#m2/0", NodeKind::Method),
                node(
                    "ExternalEndpoint:GET:/api/orders/{*}",
                    NodeKind::ExternalEndpoint,
                ),
            ],
            vec![
                edge("Method:co.C#m1/0", "Method:co.C#m2/0", EdgeKind::Calls),
                edge(
                    "Method:co.C#m2/0",
                    "ExternalEndpoint:GET:/api/orders/{*}",
                    EdgeKind::ExternalCall,
                ),
            ],
            None,
            None,
        );
        let orders = ArtifactGraph::build(
            vec![
                node("Route:GET /api/orders/{id}", NodeKind::Route),
                node("Method:or.O#h1/1", NodeKind::Method),
                node("Method:or.O#h2/0", NodeKind::Method),
            ],
            vec![
                edge(
                    "Method:or.O#h1/1",
                    "Route:GET /api/orders/{id}",
                    EdgeKind::HandlesRoute,
                ),
                edge("Method:or.O#h1/1", "Method:or.O#h2/0", EdgeKind::Calls),
            ],
            None,
            None,
        );
        let graphs = HashMap::from([
            ("checkout".to_string(), Arc::new(checkout)),
            ("orders".to_string(), Arc::new(orders)),
        ]);
        let rows = vec![http_row(
            "orders",
            "Route:GET /api/orders/{id}",
            "checkout",
            "ExternalEndpoint:GET:/api/orders/{*}",
        )];
        (graphs, rows)
    }

    fn run(
        graphs: &HashMap<String, Arc<ArtifactGraph>>,
        rows: &[ContractMatch],
        start_repo: &str,
        entry: &str,
        max_depth: u32,
        max_hops: u32,
    ) -> XTrace {
        let mut source = |repo: &str| graphs.get(repo).cloned();
        trace_across(&mut source, rows, start_repo, entry, max_depth, max_hops)
    }

    #[test]
    fn http_hop_crosses_repos_via_inverse_handles_route() {
        let (graphs, rows) = two_repo_fixture();
        let trace = run(&graphs, &rows, "checkout", "Method:co.C#m1/0", 6, 3);

        let kinds: Vec<(&str, &str)> = trace
            .steps
            .iter()
            .map(|s| (s.via.kind.as_str(), s.repo.as_str()))
            .collect();
        assert!(kinds.contains(&("CALLS", "checkout")));
        assert!(kinds.contains(&("EXTERNAL_CALL", "checkout")));
        let contract = trace
            .steps
            .iter()
            .find(|s| s.via.kind == "CONTRACT")
            .expect("contract crossing step");
        assert_eq!(contract.repo, "orders");
        assert_eq!(contract.hop, 1);
        assert_eq!(
            contract.via.match_key.as_deref(),
            Some("GET /api/orders/{*}")
        );
        // Provider-side downstream reached through the handler.
        assert!(trace
            .steps
            .iter()
            .any(|s| s.repo == "orders" && s.to == "Method:or.O#h2/0" && s.via.kind == "CALLS"));
        assert!(trace.truncated.is_empty());
    }

    #[test]
    fn event_hop_reaches_listener_in_consumer_repo() {
        let publisher = ArtifactGraph::build(
            vec![
                node("Method:a.P#send/0", NodeKind::Method),
                topic_node("KafkaTopic:orders", "orders"),
            ],
            vec![edge(
                "Method:a.P#send/0",
                "KafkaTopic:orders",
                EdgeKind::PublishesEvent,
            )],
            None,
            None,
        );
        let listener = ArtifactGraph::build(
            vec![
                node("Method:b.L#on/1", NodeKind::Method),
                node("Method:b.L#store/1", NodeKind::Method),
            ],
            vec![edge(
                "Method:b.L#on/1",
                "Method:b.L#store/1",
                EdgeKind::Calls,
            )],
            None,
            None,
        );
        let graphs = HashMap::from([
            ("svc-a".to_string(), Arc::new(publisher)),
            ("svc-b".to_string(), Arc::new(listener)),
        ]);
        let rows = vec![ContractMatch {
            kind: ContractMatchKind::KafkaTopic,
            provider_repo: "svc-a".into(),
            provider_id: "Method:a.P#send/0".into(),
            consumer_repo: "svc-b".into(),
            consumer_id: "Method:b.L#on/1".into(),
            match_key: "orders".into(),
        }];

        let mut source = |repo: &str| graphs.get(repo).cloned();
        let trace = trace_across(&mut source, &rows, "svc-a", "Method:a.P#send/0", 6, 3);
        let contract = trace
            .steps
            .iter()
            .find(|s| s.via.kind == "CONTRACT")
            .expect("event crossing");
        assert_eq!(contract.repo, "svc-b");
        assert_eq!(contract.to, "Method:b.L#on/1");
        // Listener's own downstream is walked too.
        assert!(trace
            .steps
            .iter()
            .any(|s| s.repo == "svc-b" && s.to == "Method:b.L#store/1"));
    }

    #[test]
    fn doubled_provider_rows_yield_one_crossing_each() {
        // Since the CXF dual-server route cloning, OSGi-style providers emit
        // TWO Route nodes per operation (`/v1` + `/ns/v1`) and contracts can
        // legitimately carry two provider rows for one consumer endpoint —
        // both crossings must be reported, never assumed unique.
        let (mut graphs, mut rows) = two_repo_fixture();
        let orders = ArtifactGraph::build(
            vec![
                node("Route:GET /api/orders/{id}", NodeKind::Route),
                node("Route:GET /ns/api/orders/{id}", NodeKind::Route),
                node("Method:or.O#h1/1", NodeKind::Method),
            ],
            vec![
                edge(
                    "Method:or.O#h1/1",
                    "Route:GET /api/orders/{id}",
                    EdgeKind::HandlesRoute,
                ),
                edge(
                    "Method:or.O#h1/1",
                    "Route:GET /ns/api/orders/{id}",
                    EdgeKind::HandlesRoute,
                ),
            ],
            None,
            None,
        );
        graphs.insert("orders".to_string(), Arc::new(orders));
        rows.push(ContractMatch {
            provider_id: "Route:GET /ns/api/orders/{id}".into(),
            ..rows[0].clone()
        });

        let trace = run(&graphs, &rows, "checkout", "Method:co.C#m1/0", 6, 3);
        let crossings: Vec<&str> = trace
            .steps
            .iter()
            .filter(|s| s.via.kind == "CONTRACT")
            .map(|s| s.to.as_str())
            .collect();
        assert_eq!(crossings.len(), 2, "one crossing per provider row");
    }

    #[test]
    fn max_hops_budget_truncates_instead_of_crossing() {
        let (graphs, rows) = two_repo_fixture();
        let trace = run(&graphs, &rows, "checkout", "Method:co.C#m1/0", 6, 0);
        assert!(!trace.steps.iter().any(|s| s.via.kind == "CONTRACT"));
        assert!(trace
            .truncated
            .iter()
            .any(|t| t.reason.contains("max_hops")));
    }

    #[test]
    fn missing_provider_artifacts_truncate_not_fail() {
        let (mut graphs, rows) = two_repo_fixture();
        graphs.remove("orders");
        let mut source = |repo: &str| graphs.get(repo).cloned();
        let trace = trace_across(&mut source, &rows, "checkout", "Method:co.C#m1/0", 6, 3);
        // The crossing step is still reported; the provider leg is truncated.
        assert!(trace.steps.iter().any(|s| s.via.kind == "CONTRACT"));
        assert!(trace
            .truncated
            .iter()
            .any(|t| t.repo == "orders" && t.reason.contains("unavailable")));
    }

    #[test]
    fn depth_budget_limits_each_leg() {
        let (graphs, rows) = two_repo_fixture();
        // Depth 1 from m1 only reaches m2; the ExternalEndpoint (depth 2) is
        // never reached, so no crossing happens.
        let trace = run(&graphs, &rows, "checkout", "Method:co.C#m1/0", 1, 3);
        assert!(trace.steps.iter().all(|s| s.via.kind == "CALLS"));
    }

    #[test]
    fn file_id_entry_is_traceable_but_function_entry_misses_file_attributed_calls() {
        // Phase C fallback pinned: when a TS/Py ExternalCall edge originates
        // from the *file* node, tracing from the file id crosses repos, while
        // tracing from the (untracked) function id finds nothing.
        let consumer = ArtifactGraph::build(
            vec![
                node("File:src/client.ts", NodeKind::File),
                node("Function:src/client#load/0", NodeKind::Function),
                node(
                    "ExternalEndpoint:GET:/api/orders/{*}",
                    NodeKind::ExternalEndpoint,
                ),
            ],
            vec![edge(
                "File:src/client.ts",
                "ExternalEndpoint:GET:/api/orders/{*}",
                EdgeKind::ExternalCall,
            )],
            None,
            None,
        );
        let (mut graphs, rows) = two_repo_fixture();
        graphs.insert("checkout".to_string(), Arc::new(consumer));

        let from_file = run(&graphs, &rows, "checkout", "File:src/client.ts", 6, 3);
        assert!(from_file.steps.iter().any(|s| s.via.kind == "CONTRACT"));

        let from_fn = run(
            &graphs,
            &rows,
            "checkout",
            "Function:src/client#load/0",
            6,
            3,
        );
        assert!(from_fn.steps.is_empty());
    }

    #[test]
    fn node_cap_stops_the_walk() {
        // A long chain: n0 -> n1 -> ... -> n300.
        let mut nodes = Vec::new();
        let mut edges_v = Vec::new();
        for i in 0..=300 {
            nodes.push(node(&format!("Method:m#{i}/0"), NodeKind::Method));
            if i > 0 {
                edges_v.push(edge(
                    &format!("Method:m#{}/0", i - 1),
                    &format!("Method:m#{i}/0"),
                    EdgeKind::Calls,
                ));
            }
        }
        let graphs = HashMap::from([(
            "big".to_string(),
            Arc::new(ArtifactGraph::build(nodes, edges_v, None, None)),
        )]);
        let mut source = |repo: &str| graphs.get(repo).cloned();
        let trace = trace_across(&mut source, &[], "big", "Method:m#0/0", MAX_DEPTH * 100, 3);
        assert!(trace.steps.len() <= NODE_CAP);
        assert!(trace
            .truncated
            .iter()
            .any(|t| t.reason.contains("node cap")));
    }

    #[tokio::test]
    async fn artifact_graph_loads_from_jsonl_fixtures() {
        let dir = tempfile::tempdir().unwrap();
        let nodes_path = dir.path().join("nodes.jsonl");
        let n = node("Method:a.B#c/0", NodeKind::Method);
        let e = edge("Method:a.B#c/0", "Method:a.B#d/0", EdgeKind::Calls);
        std::fs::write(
            &nodes_path,
            format!("{}\n", serde_json::to_string(&n).unwrap()),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("edges.jsonl"),
            format!("{}\n", serde_json::to_string(&e).unwrap()),
        )
        .unwrap();

        let repo = ResolvedRepo::from_entry(cih_core::RegistryEntry {
            name: "fixture".into(),
            path: dir.path().display().to_string(),
            graph_key: "fixture".into(),
            artifacts_dir: dir.path().display().to_string(),
            community_artifacts_dir: None,
            indexed_at: String::new(),
            last_git_head: None,
            stats: Default::default(),
        });
        let artifacts = Arc::new(ArtifactCache::new());
        let base = artifacts.snapshot(&repo).await.unwrap();
        assert!(!base.indexes_initialized());
        let state = XflowState::new(artifacts);
        let graph = state.graph_for(&repo).await.expect("loads");
        assert!(Arc::ptr_eq(&base, graph.snapshot()));
        assert!(base.indexes_initialized());
        assert!(graph.contains_node("Method:a.B#c/0"));
        assert_eq!(graph.out("Method:a.B#c/0").count(), 1);
        // Cached: separate graph views share the same base snapshot.
        let again = state.graph_for(&repo).await.unwrap();
        assert!(Arc::ptr_eq(graph.snapshot(), again.snapshot()));

        // Edge-only changes invalidate the graph too. The old implementation
        // watched nodes.jsonl only and retained stale adjacency indefinitely.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.path().join("edges.jsonl"), "").unwrap();
        let reloaded = state.graph_for(&repo).await.unwrap();
        assert!(!Arc::ptr_eq(graph.snapshot(), reloaded.snapshot()));
        assert_eq!(reloaded.out("Method:a.B#c/0").count(), 0);
    }

    #[test]
    fn resolve_entry_prefers_exact_id_then_unique_name() {
        let mut save_b = node("Method:a.B#save/1", NodeKind::Method);
        save_b.name = "save".into();
        let mut save_c = node("Method:a.C#save/1", NodeKind::Method);
        save_c.name = "save".into();
        let graph = ArtifactGraph::build(vec![save_b, save_c], vec![], None, None);
        assert_eq!(
            resolve_entry(&graph, "Method:a.B#save/1").unwrap(),
            "Method:a.B#save/1"
        );
        let ambiguous = resolve_entry(&graph, "save").unwrap_err();
        assert_eq!(ambiguous.len(), 2);
        assert!(resolve_entry(&graph, "nope").unwrap_err().is_empty());
    }
}
