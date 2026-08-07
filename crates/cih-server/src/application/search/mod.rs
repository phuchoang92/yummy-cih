//! Repository-scoped code search use cases.

use std::collections::{BTreeMap, BTreeSet};

use cih_core::{Edge, Node, NodeId};
use cih_graph_store::{
    CommunityInfo, GraphStore, Subgraph, SubgraphFilter, SubgraphPage, TraversalReason,
    TraversalStats, TraversalStatus, TRANSITION_PAGE_MAX,
};
use cih_search::SearchHit;
use serde::Serialize;

use crate::application::app_services::RepoContextService;
use crate::domain::error::AppError;
use crate::domain::repository::RepoSelector;

const EXPANSION_SEED_LIMIT: usize = 5;
pub(crate) const DEFAULT_EXPANSION_MAX_NODES: usize = 500;
pub(crate) const DEFAULT_EXPANSION_MAX_EDGES: usize = 1_000;
pub(crate) const DEFAULT_EXPANSION_MAX_RESPONSE_BYTES: usize = 256 * 1024;
const HARD_EXPANSION_MAX_NODES: usize = 5_000;
const HARD_EXPANSION_MAX_EDGES: usize = 10_000;
const HARD_EXPANSION_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

const ALL_EXPANSION_REASONS: &[&str] = &[
    "node_limit",
    "edge_limit",
    "response_budget",
    "endpoint_closure",
    "node_budget",
    "edge_budget",
    "backend_limit",
    "backend_completeness_unknown",
];

#[derive(Clone)]
pub(crate) struct SearchService {
    repos: RepoContextService,
}

impl SearchService {
    pub(crate) fn new(repos: RepoContextService) -> Self {
        Self { repos }
    }

    pub(crate) async fn query(&self, command: QueryCommand) -> Result<QueryOutput, AppError> {
        let repo = self
            .repos
            .resolve(RepoSelector::from_wire(&command.repo))
            .await?;
        let hits = repo
            .search
            .query_hits(&command.query, command.limit)
            .await
            .map_err(search_error)?;
        let (subgraph, expansion) =
            if let Some(limits) = command.expansion.filter(|_| !hits.is_empty()) {
                let seeds = expansion_seeds(&hits);
                let page = bounded_subgraph(repo.store.as_ref(), &seeds, limits).await?;
                let output = contain_expansion_page(&hits, seeds, page, limits)?;
                (output.subgraph, output.expansion)
            } else {
                (None, None)
            };
        Ok(QueryOutput {
            hits,
            subgraph,
            expansion,
        })
    }

    pub(crate) async fn search_code(
        &self,
        command: SearchCodeCommand,
    ) -> Result<Vec<CodeMatchOutput>, AppError> {
        let output = self
            .query(QueryCommand {
                repo: command.repo,
                query: command.query,
                limit: command.limit,
                expansion: None,
            })
            .await?;
        Ok(output
            .hits
            .into_iter()
            .map(|hit| CodeMatchOutput {
                node_id: hit.node_id.to_string(),
                kind: hit.kind.label().to_string(),
                name: hit.name,
                qualified_name: hit.qualified_name,
                file: hit.file,
                line: hit.range.start_line,
                score: hit.score,
                rank: hit.rank as u32,
            })
            .collect())
    }

    pub(crate) async fn feature_map(
        &self,
        command: FeatureMapCommand,
    ) -> Result<FeatureMapOutput, AppError> {
        let repo = self
            .repos
            .resolve(RepoSelector::from_wire(&command.repo))
            .await?;
        let hits = repo
            .search
            .query_hits(&command.query, command.limit)
            .await
            .map_err(search_error)?;
        let hit_ids: Vec<NodeId> = hits.iter().map(|hit| hit.node_id.clone()).collect();
        let memberships = repo
            .store
            .symbol_communities(&hit_ids)
            .await
            .map_err(graph_error)?;
        let community_of: BTreeMap<String, CommunityInfo> = memberships
            .into_iter()
            .map(|(node_id, community)| (node_id.to_string(), community))
            .collect();
        let mut grouped: BTreeMap<String, Vec<FeatureMapSymbol>> = BTreeMap::new();
        for hit in &hits {
            let community = community_of
                .get(hit.node_id.as_str())
                .map(|value| value.name.clone())
                .unwrap_or_else(|| "unclustered".to_string());
            grouped
                .entry(community)
                .or_default()
                .push(FeatureMapSymbol {
                    id: hit.node_id.to_string(),
                    kind: hit.kind.label().to_string(),
                    name: hit.name.clone(),
                    file: hit.file.clone(),
                    score: hit.score,
                });
        }
        let clusters = grouped
            .into_iter()
            .map(|(community, symbols)| FeatureMapCluster {
                community,
                symbol_count: symbols.len(),
                symbols,
            })
            .collect();
        Ok(FeatureMapOutput {
            query: command.query,
            total_hits: hits.len(),
            clusters,
        })
    }
}

pub(crate) struct QueryCommand {
    pub(crate) repo: String,
    pub(crate) query: String,
    pub(crate) limit: usize,
    pub(crate) expansion: Option<ExpansionLimits>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExpansionLimits {
    max_nodes: usize,
    max_edges: usize,
    max_response_bytes: usize,
}

impl ExpansionLimits {
    pub(crate) fn from_wire(max_nodes: usize, max_edges: usize, max_response_bytes: usize) -> Self {
        Self {
            max_nodes: default_and_clamp(
                max_nodes,
                DEFAULT_EXPANSION_MAX_NODES,
                HARD_EXPANSION_MAX_NODES,
            ),
            max_edges: default_and_clamp(
                max_edges,
                DEFAULT_EXPANSION_MAX_EDGES,
                HARD_EXPANSION_MAX_EDGES,
            ),
            max_response_bytes: default_and_clamp(
                max_response_bytes,
                DEFAULT_EXPANSION_MAX_RESPONSE_BYTES,
                HARD_EXPANSION_MAX_RESPONSE_BYTES,
            ),
        }
    }
}

pub(crate) struct SearchCodeCommand {
    pub(crate) repo: String,
    pub(crate) query: String,
    pub(crate) limit: usize,
}

pub(crate) struct FeatureMapCommand {
    pub(crate) repo: String,
    pub(crate) query: String,
    pub(crate) limit: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct QueryOutput {
    pub(crate) hits: Vec<SearchHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) subgraph: Option<Subgraph>,
    /// Additive containment metadata for expanded results. The legacy `hits`
    /// and `subgraph` fields retain their original shapes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expansion: Option<ExpansionBounds>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ExpansionBounds {
    /// `bounded` means the application returned every item supplied by the
    /// shared traversal; `truncated` means at least one presentation bound fired.
    pub(crate) status: &'static str,
    /// Completeness of evaluating the requested graph radius, independently
    /// from the presentation/result and byte limits represented by `status`.
    pub(crate) evaluation_status: &'static str,
    /// True only when graph evaluation and local presentation are both complete.
    pub(crate) complete: bool,
    pub(crate) locally_complete: bool,
    pub(crate) roots: Vec<NodeId>,
    pub(crate) candidate_nodes: usize,
    pub(crate) candidate_edges: usize,
    pub(crate) returned_nodes: usize,
    pub(crate) returned_edges: usize,
    pub(crate) omitted_nodes: usize,
    pub(crate) omitted_edges: usize,
    pub(crate) max_nodes: usize,
    pub(crate) max_edges: usize,
    pub(crate) max_response_bytes: usize,
    /// Exact serialized bytes for the logical application result. MCP envelope
    /// duplication is accounted for separately by transport observability.
    pub(crate) response_bytes: usize,
    pub(crate) visited_nodes: usize,
    pub(crate) expanded_edges: usize,
    pub(crate) reasons: Vec<&'static str>,
}

#[derive(Clone, Debug)]
struct ExpansionEvaluation {
    status: TraversalStatus,
    reasons: Vec<TraversalReason>,
    traversal: TraversalStats,
}

#[derive(Debug, Serialize)]
pub(crate) struct CodeMatchOutput {
    pub(crate) node_id: String,
    pub(crate) kind: String,
    pub(crate) name: String,
    pub(crate) qualified_name: Option<String>,
    pub(crate) file: String,
    pub(crate) line: u32,
    pub(crate) score: f32,
    pub(crate) rank: u32,
}

#[derive(Debug, Serialize)]
pub(crate) struct FeatureMapSymbol {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) name: String,
    pub(crate) file: String,
    pub(crate) score: f32,
}

#[derive(Debug, Serialize)]
pub(crate) struct FeatureMapCluster {
    pub(crate) community: String,
    pub(crate) symbol_count: usize,
    pub(crate) symbols: Vec<FeatureMapSymbol>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FeatureMapOutput {
    pub(crate) query: String,
    pub(crate) total_hits: usize,
    pub(crate) clusters: Vec<FeatureMapCluster>,
}

fn search_error(error: crate::ports::search_provider::SearchProviderError) -> AppError {
    let retryable = error.retryable();
    AppError::Unavailable {
        dependency: "search index",
        message: error.to_string(),
        retryable,
    }
}

fn graph_error(error: cih_graph_store::GraphStoreError) -> AppError {
    AppError::from_graph_store(error, "node")
}

fn default_and_clamp(value: usize, default: usize, maximum: usize) -> usize {
    if value == 0 {
        default
    } else {
        value.min(maximum)
    }
}

pub(crate) fn expansion_seeds(hits: &[SearchHit]) -> Vec<NodeId> {
    let mut seen = BTreeSet::new();
    hits.iter()
        .filter(|hit| seen.insert(hit.node_id.as_str().to_string()))
        .take(EXPANSION_SEED_LIMIT)
        .map(|hit| hit.node_id.clone())
        .collect()
}

#[cfg(test)]
#[cfg(test)]
pub(crate) fn contain_expansion(
    hits: &[SearchHit],
    roots: Vec<NodeId>,
    raw: Subgraph,
    limits: ExpansionLimits,
) -> Result<QueryOutput, AppError> {
    contain_expansion_with_evaluation(hits, roots, raw, limits, None)
}

pub(crate) async fn bounded_subgraph(
    store: &dyn GraphStore,
    roots: &[NodeId],
    limits: ExpansionLimits,
) -> Result<SubgraphPage, AppError> {
    let filter = SubgraphFilter {
        radius: 1,
        // All roots must be admitted so the shared kernel can produce an
        // honest evaluation. The application byte/result pass below still
        // applies the caller's exact max_nodes presentation limit.
        node_limit: limits.max_nodes.max(roots.len()),
        edge_limit: limits.max_edges,
        transition_page_limit: limits.max_edges.clamp(1, TRANSITION_PAGE_MAX),
    };
    store
        .subgraph_bounded(roots, &filter)
        .await
        .map_err(graph_error)
}

pub(crate) fn contain_expansion_page(
    hits: &[SearchHit],
    roots: Vec<NodeId>,
    page: SubgraphPage,
    limits: ExpansionLimits,
) -> Result<QueryOutput, AppError> {
    let evaluation = ExpansionEvaluation {
        status: page.status,
        reasons: page.reasons,
        traversal: page.traversal,
    };
    let raw = Subgraph {
        nodes: page.nodes.into_iter().map(|item| item.node).collect(),
        edges: page.edges,
    };
    contain_expansion_with_evaluation(hits, roots, raw, limits, Some(evaluation))
}

fn contain_expansion_with_evaluation(
    hits: &[SearchHit],
    roots: Vec<NodeId>,
    raw: Subgraph,
    limits: ExpansionLimits,
    evaluation: Option<ExpansionEvaluation>,
) -> Result<QueryOutput, AppError> {
    let nodes = deduplicate_nodes(raw.nodes, &roots)?;
    let edges = deduplicate_edges(raw.edges)?;
    let candidate_nodes = nodes.len();
    let candidate_edges = edges.len();

    // Reserve for the largest possible metadata representation first. Item
    // sizes are then additive inside the two JSON arrays, keeping this pass
    // linear even for an unexpectedly large adapter response.
    let empty = Subgraph {
        nodes: Vec::new(),
        edges: Vec::new(),
    };
    let maximum_metadata = maximum_expansion_metadata(&roots, limits);
    let reserved = serialized_len(&QueryOutputRef {
        hits,
        subgraph: Some(&empty),
        expansion: Some(&maximum_metadata),
    })?;
    let mut remaining_bytes = limits.max_response_bytes.saturating_sub(reserved);

    let mut selected_nodes = Vec::new();
    let mut selected_node_ids = BTreeSet::new();
    let mut node_limit_reached = false;
    let mut response_budget_reached = reserved > limits.max_response_bytes;
    for node in nodes {
        if selected_nodes.len() >= limits.max_nodes {
            node_limit_reached = true;
            continue;
        }
        let item_bytes = serialized_len(&node)? + usize::from(!selected_nodes.is_empty());
        if item_bytes > remaining_bytes {
            response_budget_reached = true;
            continue;
        }
        remaining_bytes -= item_bytes;
        selected_node_ids.insert(node.id.as_str().to_string());
        selected_nodes.push(node);
    }

    let mut selected_edges = Vec::new();
    let mut edge_limit_reached = false;
    let mut endpoint_closure_applied = false;
    for edge in edges {
        if !selected_node_ids.contains(edge.src.as_str())
            || !selected_node_ids.contains(edge.dst.as_str())
        {
            endpoint_closure_applied = true;
            continue;
        }
        if selected_edges.len() >= limits.max_edges {
            edge_limit_reached = true;
            continue;
        }
        let item_bytes = serialized_len(&edge)? + usize::from(!selected_edges.is_empty());
        if item_bytes > remaining_bytes {
            response_budget_reached = true;
            continue;
        }
        remaining_bytes -= item_bytes;
        selected_edges.push(edge);
    }

    let subgraph = Subgraph {
        nodes: selected_nodes,
        edges: selected_edges,
    };
    let mut expansion = expansion_metadata(
        roots,
        candidate_nodes,
        candidate_edges,
        &subgraph,
        limits,
        node_limit_reached,
        edge_limit_reached,
        response_budget_reached,
        endpoint_closure_applied,
        evaluation.as_ref(),
    );
    let mut output = QueryOutput {
        hits: hits.to_vec(),
        subgraph: Some(subgraph),
        expansion: Some(expansion.clone()),
    };

    // `response_bytes` is part of the response itself. Iterate until its digit
    // width is stable, then enforce the requested logical response ceiling.
    for _ in 0..4 {
        let bytes = serialized_len(&output)?;
        if expansion.response_bytes == bytes {
            break;
        }
        expansion.response_bytes = bytes;
        output.expansion = Some(expansion.clone());
    }
    let response_bytes = serialized_len(&output)?;
    if response_bytes > limits.max_response_bytes {
        return Err(AppError::InvalidInput {
            field: "max_response_bytes",
            message: format!(
                "the {max}-byte budget cannot fit the search hits and expansion metadata; \
                 increase max_response_bytes or lower limit",
                max = limits.max_response_bytes
            ),
        });
    }
    if let Some(bounds) = output.expansion.as_mut() {
        bounds.response_bytes = response_bytes;
    }
    Ok(output)
}

#[derive(Serialize)]
struct QueryOutputRef<'a> {
    hits: &'a [SearchHit],
    #[serde(skip_serializing_if = "Option::is_none")]
    subgraph: Option<&'a Subgraph>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expansion: Option<&'a ExpansionBounds>,
}

fn maximum_expansion_metadata(roots: &[NodeId], limits: ExpansionLimits) -> ExpansionBounds {
    ExpansionBounds {
        status: "truncated",
        complete: false,
        locally_complete: false,
        roots: roots.to_vec(),
        candidate_nodes: usize::MAX,
        candidate_edges: usize::MAX,
        returned_nodes: usize::MAX,
        returned_edges: usize::MAX,
        omitted_nodes: usize::MAX,
        omitted_edges: usize::MAX,
        max_nodes: limits.max_nodes,
        max_edges: limits.max_edges,
        max_response_bytes: limits.max_response_bytes,
        response_bytes: usize::MAX,
        visited_nodes: usize::MAX,
        expanded_edges: usize::MAX,
        evaluation_status: "inconclusive",
        reasons: ALL_EXPANSION_REASONS.to_vec(),
    }
}

#[allow(clippy::too_many_arguments)]
fn expansion_metadata(
    roots: Vec<NodeId>,
    candidate_nodes: usize,
    candidate_edges: usize,
    subgraph: &Subgraph,
    limits: ExpansionLimits,
    node_limit_reached: bool,
    edge_limit_reached: bool,
    response_budget_reached: bool,
    endpoint_closure_applied: bool,
    evaluation: Option<&ExpansionEvaluation>,
) -> ExpansionBounds {
    let omitted_nodes = candidate_nodes.saturating_sub(subgraph.nodes.len());
    let omitted_edges = candidate_edges.saturating_sub(subgraph.edges.len());
    let locally_complete = omitted_nodes == 0 && omitted_edges == 0;
    let mut reasons = Vec::new();
    if node_limit_reached {
        reasons.push("node_limit");
    }
    if edge_limit_reached {
        reasons.push("edge_limit");
    }
    if response_budget_reached {
        reasons.push("response_budget");
    }
    if endpoint_closure_applied {
        reasons.push("endpoint_closure");
    }
    if let Some(evaluation) = evaluation {
        for reason in &evaluation.reasons {
            let reason = traversal_reason_label(*reason);
            if !reasons.contains(&reason) {
                reasons.push(reason);
            }
        }
    } else {
        reasons.push("backend_completeness_unknown");
    }
    let evaluation_status = evaluation
        .map(|value| traversal_status_label(value.status))
        .unwrap_or("unknown");
    let evaluation_complete =
        evaluation.is_some_and(|value| value.status == TraversalStatus::Complete);
    let traversal = evaluation.map(|value| &value.traversal);
    ExpansionBounds {
        status: if locally_complete {
            "bounded"
        } else {
            "truncated"
        },
        evaluation_status,
        complete: evaluation_complete && locally_complete,
        locally_complete,
        roots,
        candidate_nodes,
        candidate_edges,
        returned_nodes: subgraph.nodes.len(),
        returned_edges: subgraph.edges.len(),
        omitted_nodes,
        omitted_edges,
        max_nodes: limits.max_nodes,
        max_edges: limits.max_edges,
        max_response_bytes: limits.max_response_bytes,
        response_bytes: 0,
        visited_nodes: traversal.map_or(0, |stats| stats.visited_nodes),
        expanded_edges: traversal.map_or(0, |stats| stats.expanded_edges),
        reasons,
    }
}

fn traversal_status_label(status: TraversalStatus) -> &'static str {
    match status {
        TraversalStatus::Complete => "complete",
        TraversalStatus::Truncated => "truncated",
        TraversalStatus::Inconclusive => "inconclusive",
    }
}

fn traversal_reason_label(reason: TraversalReason) -> &'static str {
    match reason {
        TraversalReason::ResultLimit => "result_limit",
        TraversalReason::NodeBudget => "node_budget",
        TraversalReason::EdgeBudget => "edge_budget",
        TraversalReason::BackendLimit => "backend_limit",
    }
}

fn deduplicate_nodes(nodes: Vec<Node>, roots: &[NodeId]) -> Result<Vec<Node>, AppError> {
    let mut by_id: BTreeMap<String, Node> = BTreeMap::new();
    for node in nodes {
        match by_id.entry(node.id.as_str().to_string()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(node);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if serialized(&node)? < serialized(entry.get())? {
                    entry.insert(node);
                }
            }
        }
    }

    // Keep roots in search-rank order, then use stable node-id order for the
    // shared aggregate. This makes tight budgets reproducible.
    let mut ordered = Vec::with_capacity(by_id.len());
    for root in roots {
        if let Some(node) = by_id.remove(root.as_str()) {
            ordered.push(node);
        }
    }
    ordered.extend(by_id.into_values());
    Ok(ordered)
}

fn deduplicate_edges(edges: Vec<Edge>) -> Result<Vec<Edge>, AppError> {
    let mut by_key: BTreeMap<(String, String, String), Edge> = BTreeMap::new();
    for edge in edges {
        let key = (
            edge.src.as_str().to_string(),
            edge.dst.as_str().to_string(),
            edge.kind.cypher_label().to_string(),
        );
        match by_key.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(edge);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let confidence_order = edge.confidence.total_cmp(&entry.get().confidence);
                if confidence_order.is_gt()
                    || (confidence_order.is_eq() && serialized(&edge)? < serialized(entry.get())?)
                {
                    entry.insert(edge);
                }
            }
        }
    }
    Ok(by_key.into_values().collect())
}

fn serialized<T: Serialize>(value: &T) -> Result<Vec<u8>, AppError> {
    serde_json::to_vec(value).map_err(|error| AppError::Unavailable {
        dependency: "response serializer",
        message: error.to_string(),
        retryable: false,
    })
}

fn serialized_len<T: Serialize>(value: &T) -> Result<usize, AppError> {
    serialized(value).map(|bytes| bytes.len())
}

#[cfg(test)]
mod tests {
    use cih_core::{EdgeKind, NodeKind, Range};

    use super::*;

    fn node(id: &str, name: impl Into<String>) -> Node {
        Node {
            id: NodeId::new(id),
            kind: NodeKind::Method,
            name: name.into(),
            qualified_name: None,
            file: "src/Test.java".to_string(),
            range: Range::default(),
            props: None,
        }
    }

    fn edge(src: &str, dst: &str, confidence: f32) -> Edge {
        Edge::new(
            NodeId::new(src),
            NodeId::new(dst),
            EdgeKind::Calls,
            confidence,
            format!("confidence-{confidence}"),
        )
    }

    fn hit(id: &str, rank: usize) -> SearchHit {
        SearchHit {
            node_id: NodeId::new(id),
            kind: NodeKind::Method,
            name: id.to_string(),
            qualified_name: None,
            file: "src/Test.java".to_string(),
            range: Range::default(),
            score: 1.0 / rank as f32,
            rank,
            sources: vec!["bm25".to_string()],
            bm25_score: Some(1.0),
            semantic_score: None,
        }
    }

    fn limits(max_nodes: usize, max_edges: usize, max_response_bytes: usize) -> ExpansionLimits {
        ExpansionLimits {
            max_nodes,
            max_edges,
            max_response_bytes,
        }
    }

    #[test]
    fn expansion_deduplicates_and_removes_dangling_edges() {
        let hits = vec![hit("Method:A", 1), hit("Method:B", 2)];
        let roots = expansion_seeds(&hits);
        let raw = Subgraph {
            nodes: vec![
                node("Method:B", "B"),
                node("Method:A", "A-z"),
                node("Method:A", "A-a"),
            ],
            edges: vec![
                edge("Method:A", "Method:B", 0.2),
                edge("Method:A", "Method:B", 0.9),
                edge("Method:A", "Method:Missing", 1.0),
            ],
        };

        let output = contain_expansion(&hits, roots, raw, limits(100, 100, 64 * 1024))
            .expect("contained expansion");
        let graph = output.subgraph.expect("expanded graph");
        let bounds = output.expansion.expect("expansion bounds");

        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.nodes[0].id.as_str(), "Method:A");
        assert_eq!(graph.nodes[0].name, "A-a");
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].confidence, 0.9);
        assert!(graph.edges.iter().all(|edge| {
            graph.nodes.iter().any(|node| node.id == edge.src)
                && graph.nodes.iter().any(|node| node.id == edge.dst)
        }));
        assert_eq!(bounds.candidate_nodes, 2);
        assert_eq!(bounds.candidate_edges, 2);
        assert_eq!(bounds.omitted_edges, 1);
        assert!(bounds.reasons.contains(&"endpoint_closure"));
        assert!(!bounds.complete);
    }

    #[test]
    fn top_five_unique_roots_share_one_aggregate_budget() {
        let hits = vec![
            hit("Method:A", 1),
            hit("Method:A", 2),
            hit("Method:B", 3),
            hit("Method:C", 4),
            hit("Method:D", 5),
            hit("Method:E", 6),
            hit("Method:F", 7),
        ];
        let roots = expansion_seeds(&hits);
        assert_eq!(
            roots.iter().map(NodeId::as_str).collect::<Vec<_>>(),
            vec!["Method:A", "Method:B", "Method:C", "Method:D", "Method:E"]
        );
        let raw = Subgraph {
            nodes: ["A", "B", "C", "D", "E", "F"]
                .into_iter()
                .map(|suffix| node(&format!("Method:{suffix}"), suffix))
                .collect(),
            edges: vec![
                edge("Method:A", "Method:B", 1.0),
                edge("Method:B", "Method:C", 1.0),
                edge("Method:C", "Method:D", 1.0),
            ],
        };

        let output = contain_expansion(&hits, roots, raw, limits(3, 1, 64 * 1024))
            .expect("contained expansion");
        let graph = output.subgraph.expect("expanded graph");
        let bounds = output.expansion.expect("expansion bounds");

        assert_eq!(graph.nodes.len(), 3, "node cap is aggregate, not per root");
        assert_eq!(graph.edges.len(), 1, "edge cap is aggregate, not per root");
        assert_eq!(bounds.roots.len(), 5);
        assert_eq!(bounds.returned_nodes, 3);
        assert_eq!(bounds.returned_edges, 1);
        assert!(bounds.reasons.contains(&"node_limit"));
        assert!(bounds.reasons.contains(&"edge_limit"));
        assert_eq!(bounds.status, "truncated");
    }

    #[test]
    fn response_budget_is_enforced_and_reported_exactly() {
        let hits = vec![hit("Method:A", 1)];
        let roots = expansion_seeds(&hits);
        let raw = Subgraph {
            nodes: (0..10)
                .map(|index| {
                    node(
                        &format!("Method:{index}"),
                        format!("node-{index}-{}", "x".repeat(3_000)),
                    )
                })
                .collect(),
            edges: Vec::new(),
        };

        let output = contain_expansion(&hits, roots, raw, limits(100, 100, 4 * 1024))
            .expect("contained expansion");
        let bytes = serde_json::to_vec(&output).expect("serialize output").len();
        let bounds = output.expansion.as_ref().expect("expansion bounds");

        assert!(bytes <= 4 * 1024);
        assert_eq!(bounds.response_bytes, bytes);
        assert!(bounds.returned_nodes < bounds.candidate_nodes);
        assert!(bounds.reasons.contains(&"response_budget"));
        assert!(!bounds.complete);
    }

    #[test]
    fn containment_is_deterministic_across_adapter_row_order() {
        let hits = vec![hit("Method:A", 1), hit("Method:B", 2)];
        let roots = expansion_seeds(&hits);
        let first = Subgraph {
            nodes: vec![node("Method:B", "B"), node("Method:A", "A")],
            edges: vec![
                edge("Method:A", "Method:B", 0.2),
                edge("Method:A", "Method:B", 0.9),
            ],
        };
        let second = Subgraph {
            nodes: vec![node("Method:A", "A"), node("Method:B", "B")],
            edges: vec![
                edge("Method:A", "Method:B", 0.9),
                edge("Method:A", "Method:B", 0.2),
            ],
        };

        let first = contain_expansion(&hits, roots.clone(), first, limits(100, 100, 64 * 1024))
            .expect("first result");
        let second = contain_expansion(&hits, roots, second, limits(100, 100, 64 * 1024))
            .expect("second result");

        assert_eq!(
            serde_json::to_value(first.subgraph).expect("first JSON"),
            serde_json::to_value(second.subgraph).expect("second JSON")
        );
    }

    #[test]
    fn shared_traversal_completeness_is_preserved_separately_from_presentation() {
        let hits = vec![hit("Method:A", 1), hit("Method:B", 2)];
        let roots = expansion_seeds(&hits);
        let page = SubgraphPage {
            roots: roots.clone(),
            nodes: vec![
                cih_graph_store::SubgraphNode {
                    node: node("Method:A", "A"),
                    depth: 0,
                    root: true,
                },
                cih_graph_store::SubgraphNode {
                    node: node("Method:B", "B"),
                    depth: 0,
                    root: true,
                },
            ],
            edges: vec![edge("Method:A", "Method:B", 1.0)],
            status: TraversalStatus::Complete,
            reasons: Vec::new(),
            has_more: false,
            traversal: TraversalStats {
                visited_nodes: 2,
                expanded_edges: 1,
                truncated: false,
            },
        };
        let output = contain_expansion_page(&hits, roots, page, limits(10, 10, 64 * 1024))
            .expect("complete bounded expansion");
        let bounds = output.expansion.expect("expansion bounds");
        assert_eq!(bounds.evaluation_status, "complete");
        assert!(bounds.complete);
        assert_eq!(bounds.visited_nodes, 2);
        assert_eq!(bounds.expanded_edges, 1);

        let page = SubgraphPage {
            roots: vec![NodeId::new("Method:A")],
            nodes: vec![cih_graph_store::SubgraphNode {
                node: node("Method:A", "A"),
                depth: 0,
                root: true,
            }],
            edges: Vec::new(),
            status: TraversalStatus::Inconclusive,
            reasons: vec![TraversalReason::EdgeBudget],
            has_more: true,
            traversal: TraversalStats {
                visited_nodes: 1,
                expanded_edges: 10,
                truncated: true,
            },
        };
        // The kernel's explicit budget reason is retained even if every item
        // supplied to the local presentation pass fits.
        let page_roots = page.roots.clone();
        let output = contain_expansion_page(
            &[hit("Method:A", 1)],
            page_roots,
            page,
            limits(10, 10, 64 * 1024),
        )
        .expect("inconclusive bounded expansion");
        let bounds = output.expansion.expect("expansion bounds");
        assert_eq!(bounds.evaluation_status, "inconclusive");
        assert!(!bounds.complete);
        assert!(bounds.reasons.contains(&"edge_budget"));
    }

    #[test]
    fn query_args_keep_zero_as_the_backward_compatible_default_sentinel() {
        let args: crate::transport::mcp::args::QueryArgs =
            serde_json::from_value(serde_json::json!({"q": "auth", "expand": true}))
                .expect("legacy query args");

        assert_eq!(args.max_nodes, 0);
        assert_eq!(args.max_edges, 0);
        assert_eq!(args.max_response_bytes, 0);
        assert_eq!(
            ExpansionLimits::from_wire(args.max_nodes, args.max_edges, args.max_response_bytes,),
            limits(
                DEFAULT_EXPANSION_MAX_NODES,
                DEFAULT_EXPANSION_MAX_EDGES,
                DEFAULT_EXPANSION_MAX_RESPONSE_BYTES,
            )
        );
    }

    #[test]
    fn wire_limits_are_hard_capped() {
        assert_eq!(
            ExpansionLimits::from_wire(usize::MAX, usize::MAX, usize::MAX),
            limits(
                HARD_EXPANSION_MAX_NODES,
                HARD_EXPANSION_MAX_EDGES,
                HARD_EXPANSION_MAX_RESPONSE_BYTES,
            )
        );
    }
}
