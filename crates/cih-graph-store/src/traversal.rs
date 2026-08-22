//! Backend-neutral execution traversal.
//!
//! Adapters expose deterministic, bounded one-hop transitions. This module
//! owns depth semantics, cycle handling, filtering, pagination, path
//! reconstruction, and resource budgets so both graph backends behave alike.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind};

use crate::{
    risk_from_fanout, Direction, ExecutionTransition, FlowEdge, FlowFilter, FlowHop, FlowNode,
    FlowPage, GraphStore, GraphStoreError, Impact, ImpactFilter, ImpactNode, ImpactPage,
    InterceptingAdvice, Interception, PathAccess, PathEdge, PathFilter, PathInfo, PathPage, Result,
    StoredTransition, SubgraphFilter, SubgraphNode, SubgraphPage, TransitionBatch, TransitionQuery,
    TraversalReason, TraversalStats, TraversalStatus, EXECUTION_BATCH_SIZE, FLOW_VISIBLE_WINDOW,
    TRAVERSAL_EDGE_BUDGET, TRAVERSAL_NODE_BUDGET,
};

#[derive(Clone, Copy)]
struct TraversalLimits {
    node_budget: usize,
    edge_budget: usize,
    batch_size: usize,
    visible_window: usize,
}

impl TraversalLimits {
    const PRODUCTION: Self = Self {
        node_budget: TRAVERSAL_NODE_BUDGET,
        edge_budget: TRAVERSAL_EDGE_BUDGET,
        batch_size: EXECUTION_BATCH_SIZE,
        visible_window: FLOW_VISIBLE_WINDOW,
    };
}

/// Narrow internal port for the shared traversal. Keeping the BFS dependent on
/// only its three reads makes its budget behavior directly testable without a
/// backend or a fake implementation of the entire graph-store API.
#[async_trait::async_trait]
trait TraversalSource: Send + Sync {
    async fn get_node(&self, id: &NodeId) -> Result<Option<Node>>;
    async fn execution_transitions(
        &self,
        ids: &[NodeId],
        include_data: bool,
        limit: usize,
    ) -> Result<Vec<ExecutionTransition>>;
    async fn interceptions_for_methods(&self, ids: &[NodeId]) -> Result<Vec<Interception>>;
}

#[async_trait::async_trait]
impl<S: GraphStore + ?Sized> TraversalSource for S {
    async fn get_node(&self, id: &NodeId) -> Result<Option<Node>> {
        GraphStore::get_node(self, id).await
    }

    async fn execution_transitions(
        &self,
        ids: &[NodeId],
        include_data: bool,
        limit: usize,
    ) -> Result<Vec<ExecutionTransition>> {
        GraphStore::execution_transitions(self, ids, include_data, limit).await
    }

    async fn interceptions_for_methods(&self, ids: &[NodeId]) -> Result<Vec<Interception>> {
        GraphStore::interceptions_for_methods(self, ids).await
    }
}

#[derive(Clone)]
struct ReachedNode {
    node: Node,
    depth: u32,
    parent: Option<NodeId>,
    via: Option<ExecutionTransition>,
    is_accessor: bool,
}

#[derive(Clone)]
struct Predecessor {
    source: NodeId,
    edge: PathEdge,
}

/// Narrow port for the stored-orientation kernel. Keeping it separate from
/// logical execution traversal prevents route/listener reversal from leaking
/// into impact and browser subgraphs.
#[async_trait::async_trait]
trait StoredTraversalSource: Send + Sync {
    async fn stored_get_node(&self, id: &NodeId) -> Result<Option<Node>>;
    async fn batched_transitions(
        &self,
        sources: &[NodeId],
        query: &TransitionQuery,
    ) -> Result<TransitionBatch>;
}

#[async_trait::async_trait]
impl<S: GraphStore + ?Sized> StoredTraversalSource for S {
    async fn stored_get_node(&self, id: &NodeId) -> Result<Option<Node>> {
        GraphStore::get_node(self, id).await
    }

    async fn batched_transitions(
        &self,
        sources: &[NodeId],
        query: &TransitionQuery,
    ) -> Result<TransitionBatch> {
        GraphStore::batched_transitions(self, sources, query).await
    }
}

#[derive(Default)]
struct LayerExpansion {
    transitions: Vec<StoredTransition>,
    reasons: Vec<TraversalReason>,
    examined_edges: usize,
}

pub(crate) async fn impact<S: GraphStore + ?Sized>(
    store: &S,
    root_id: &NodeId,
    filter: &ImpactFilter,
) -> Result<ImpactPage> {
    impact_with_source(store, root_id, filter).await
}

async fn impact_with_source<S: StoredTraversalSource + ?Sized>(
    store: &S,
    root_id: &NodeId,
    filter: &ImpactFilter,
) -> Result<ImpactPage> {
    filter.validate()?;
    let root = store
        .stored_get_node(root_id)
        .await?
        .ok_or_else(|| GraphStoreError::NotFound(root_id.to_string()))?;
    let mut nodes = HashMap::<NodeId, Node>::new();
    nodes.insert(root_id.clone(), root);
    let mut depths = HashMap::<NodeId, u32>::new();
    depths.insert(root_id.clone(), 0);
    let mut parents = HashMap::<NodeId, NodeId>::new();
    let mut frontier = vec![root_id.clone()];
    let mut traversal = TraversalStats {
        visited_nodes: 1,
        ..TraversalStats::default()
    };
    let mut reasons = Vec::new();

    for depth in 1..=filter.max_depth {
        if frontier.is_empty() || !reasons.is_empty() {
            break;
        }
        frontier.sort_by(|a, b| node_id_cmp(a, b, &nodes));
        let remaining_edges = filter.edge_budget.saturating_sub(traversal.expanded_edges);
        let mut layer = expand_stored_layer(
            store,
            &frontier,
            filter.direction,
            &[EdgeKind::Calls],
            filter.transition_page_limit,
            remaining_edges,
        )
        .await?;
        traversal.expanded_edges = traversal
            .expanded_edges
            .saturating_add(layer.examined_edges);
        layer.transitions.sort_by(|a, b| {
            a.target
                .name
                .cmp(&b.target.name)
                .then_with(|| a.target.id.as_str().cmp(b.target.id.as_str()))
                .then_with(|| node_id_cmp(&a.source, &b.source, &nodes))
                .then_with(|| stored_transition_cmp(a, b))
        });

        let mut next = Vec::new();
        let mut node_budget_hit = false;
        for transition in layer.transitions {
            let target_id = transition.target.id.clone();
            if depths.contains_key(&target_id) {
                continue;
            }
            if nodes.len() >= filter.node_budget {
                node_budget_hit = true;
                continue;
            }
            parents.insert(target_id.clone(), transition.source);
            depths.insert(target_id.clone(), depth);
            nodes.insert(target_id.clone(), transition.target);
            next.push(target_id);
        }
        if node_budget_hit {
            push_reason(&mut reasons, TraversalReason::NodeBudget);
        }
        for reason in layer.reasons {
            push_reason(&mut reasons, reason);
        }
        traversal.visited_nodes = nodes.len();
        frontier = next;
    }

    let mut affected: Vec<ImpactNode> = nodes
        .into_values()
        .filter_map(|node| {
            let depth = depths.get(&node.id).copied()?;
            (depth > 0).then(|| ImpactNode {
                parent_id: parents.get(&node.id).cloned(),
                id: node.id,
                depth,
                via: EdgeKind::Calls.cypher_label().to_string(),
                name: node.name,
                kind: node.kind.label().to_string(),
            })
        })
        .collect();
    affected.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
    let evaluated_affected = affected.len();
    let result_limited = affected.len() > filter.result_limit;
    if result_limited {
        affected.truncate(filter.result_limit);
    }
    let risk_exact = reasons.is_empty();
    let status = if !risk_exact {
        TraversalStatus::Inconclusive
    } else if result_limited {
        push_reason(&mut reasons, TraversalReason::ResultLimit);
        TraversalStatus::Truncated
    } else {
        TraversalStatus::Complete
    };
    traversal.truncated = status != TraversalStatus::Complete;
    let impact = Impact {
        root: root_id.clone(),
        direction: filter.direction,
        risk: risk_from_fanout(evaluated_affected).to_string(),
        affected,
        has_more: status != TraversalStatus::Complete,
        backend_limit: filter.result_limit,
        traversal,
        risk_exact,
        risk_lower_bound: !risk_exact,
    };
    Ok(ImpactPage {
        impact,
        status,
        reasons,
        evaluated_affected,
    })
}

pub(crate) async fn subgraph<S: GraphStore + ?Sized>(
    store: &S,
    seeds: &[NodeId],
    filter: &SubgraphFilter,
) -> Result<SubgraphPage> {
    subgraph_with_source(store, seeds, filter).await
}

async fn subgraph_with_source<S: StoredTraversalSource + ?Sized>(
    store: &S,
    seeds: &[NodeId],
    filter: &SubgraphFilter,
) -> Result<SubgraphPage> {
    filter.validate()?;
    let mut roots = seeds.to_vec();
    roots.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    roots.dedup();
    if roots.len() > filter.node_limit {
        return Err(GraphStoreError::InvalidInput(format!(
            "{} distinct subgraph roots exceed node_limit {}",
            roots.len(),
            filter.node_limit
        )));
    }

    let root_set: HashSet<NodeId> = roots.iter().cloned().collect();
    let mut nodes = HashMap::<NodeId, Node>::new();
    let mut depths = HashMap::<NodeId, u32>::new();
    for root_id in &roots {
        let node = store
            .stored_get_node(root_id)
            .await?
            .ok_or_else(|| GraphStoreError::NotFound(root_id.to_string()))?;
        depths.insert(root_id.clone(), 0);
        nodes.insert(root_id.clone(), node);
    }
    let mut traversal = TraversalStats {
        visited_nodes: nodes.len(),
        ..TraversalStats::default()
    };
    let mut frontier = roots.clone();
    let mut selected_edges = HashMap::<(NodeId, NodeId, EdgeKind), Edge>::new();
    let mut reasons = Vec::new();

    for depth in 1..=filter.radius {
        if frontier.is_empty() || !reasons.is_empty() {
            break;
        }
        frontier.sort_by(|a, b| node_id_cmp(a, b, &nodes));
        let remaining_edges = filter.edge_limit.saturating_sub(traversal.expanded_edges);
        let mut layer = expand_stored_layer(
            store,
            &frontier,
            Direction::Both,
            &filter.edge_kinds,
            filter.transition_page_limit,
            remaining_edges,
        )
        .await?;
        traversal.expanded_edges = traversal
            .expanded_edges
            .saturating_add(layer.examined_edges);
        layer.transitions.sort_by(stored_transition_cmp);
        let mut next = Vec::new();
        let mut node_budget_hit = false;
        for transition in layer.transitions {
            merge_stored_edge(&mut selected_edges, transition.edge);
            let target_id = transition.target.id.clone();
            if depths.contains_key(&target_id) {
                continue;
            }
            if nodes.len() >= filter.node_limit {
                node_budget_hit = true;
                continue;
            }
            depths.insert(target_id.clone(), depth);
            nodes.insert(target_id.clone(), transition.target);
            next.push(target_id);
        }
        if node_budget_hit {
            push_reason(&mut reasons, TraversalReason::NodeBudget);
        }
        for reason in layer.reasons.drain(..) {
            push_reason(&mut reasons, reason);
        }
        traversal.visited_nodes = nodes.len();
        frontier = next;
    }

    let selected_ids: HashSet<NodeId> = nodes.keys().cloned().collect();
    let mut edges: Vec<Edge> = selected_edges
        .into_values()
        .filter(|edge| selected_ids.contains(&edge.src) && selected_ids.contains(&edge.dst))
        .collect();
    edges.sort_by(|a, b| {
        a.src
            .as_str()
            .cmp(b.src.as_str())
            .then_with(|| a.dst.as_str().cmp(b.dst.as_str()))
            .then_with(|| a.kind.cypher_label().cmp(b.kind.cypher_label()))
    });
    if edges.len() > filter.edge_limit {
        edges.truncate(filter.edge_limit);
        push_reason(&mut reasons, TraversalReason::EdgeBudget);
    }
    let mut page_nodes: Vec<SubgraphNode> = nodes
        .into_values()
        .map(|node| SubgraphNode {
            depth: depths.get(&node.id).copied().unwrap_or(0),
            root: root_set.contains(&node.id),
            node,
        })
        .collect();
    page_nodes.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then_with(|| a.node.name.cmp(&b.node.name))
            .then_with(|| a.node.id.as_str().cmp(b.node.id.as_str()))
    });
    let status = if reasons.is_empty() {
        TraversalStatus::Complete
    } else {
        // Every reason emitted by the subgraph kernel is a work/backend
        // budget. Once one is hit, the requested radius has not been fully
        // evaluated, so the honest outcome is inconclusive rather than a
        // complete evaluation with a merely shortened presentation page.
        TraversalStatus::Inconclusive
    };
    traversal.truncated = status != TraversalStatus::Complete;
    Ok(SubgraphPage {
        roots,
        nodes: page_nodes,
        edges,
        status,
        has_more: status != TraversalStatus::Complete,
        reasons,
        traversal,
    })
}

async fn expand_stored_layer<S: StoredTraversalSource + ?Sized>(
    store: &S,
    frontier: &[NodeId],
    direction: Direction,
    edge_kinds: &[EdgeKind],
    page_limit: usize,
    edge_budget: usize,
) -> Result<LayerExpansion> {
    let mut expansion = LayerExpansion::default();
    if edge_budget == 0 && !frontier.is_empty() {
        expansion.reasons.push(TraversalReason::EdgeBudget);
        return Ok(expansion);
    }

    for sources in frontier.chunks(EXECUTION_BATCH_SIZE) {
        let mut after = None;
        loop {
            let remaining = edge_budget.saturating_sub(expansion.transitions.len());
            if remaining == 0 {
                push_reason(&mut expansion.reasons, TraversalReason::EdgeBudget);
                break;
            }
            let query = TransitionQuery {
                direction,
                edge_kinds: edge_kinds.to_vec(),
                page_limit: page_limit.min(remaining),
                after: after.clone(),
            };
            let batch = store.batched_transitions(sources, &query).await?;
            if batch.transitions.len() > query.page_limit {
                push_reason(&mut expansion.reasons, TraversalReason::BackendLimit);
                break;
            }
            expansion.examined_edges = expansion
                .examined_edges
                .saturating_add(batch.transitions.len());
            expansion.transitions.extend(batch.transitions);
            match (batch.backend_limited, batch.next_cursor) {
                (false, None) => break,
                (true, Some(next)) => {
                    if after.as_ref() == Some(&next) {
                        push_reason(&mut expansion.reasons, TraversalReason::BackendLimit);
                        break;
                    }
                    after = Some(next);
                    if expansion.transitions.len() >= edge_budget {
                        push_reason(&mut expansion.reasons, TraversalReason::EdgeBudget);
                        break;
                    }
                }
                _ => {
                    push_reason(&mut expansion.reasons, TraversalReason::BackendLimit);
                    break;
                }
            }
        }
        if !expansion.reasons.is_empty() {
            break;
        }
    }
    expansion.transitions.sort_by(stored_transition_cmp);
    expansion.transitions.dedup_by(|a, b| {
        a.source == b.source
            && a.target.id == b.target.id
            && a.edge.src == b.edge.src
            && a.edge.dst == b.edge.dst
            && a.edge.kind == b.edge.kind
            && a.traversed_reverse == b.traversed_reverse
    });
    Ok(expansion)
}

fn stored_transition_cmp(a: &StoredTransition, b: &StoredTransition) -> Ordering {
    a.source
        .as_str()
        .cmp(b.source.as_str())
        .then_with(|| a.target.name.cmp(&b.target.name))
        .then_with(|| a.target.id.as_str().cmp(b.target.id.as_str()))
        .then_with(|| a.edge.kind.cypher_label().cmp(b.edge.kind.cypher_label()))
        .then_with(|| a.traversed_reverse.cmp(&b.traversed_reverse))
        .then_with(|| a.stored_edge_token.cmp(&b.stored_edge_token))
}

fn merge_stored_edge(edges: &mut HashMap<(NodeId, NodeId, EdgeKind), Edge>, candidate: Edge) {
    let key = (candidate.src.clone(), candidate.dst.clone(), candidate.kind);
    match edges.get_mut(&key) {
        Some(current) => {
            if candidate.confidence > current.confidence {
                current.confidence = candidate.confidence;
            }
            if current.reason.is_empty()
                || (!candidate.reason.is_empty() && candidate.reason < current.reason)
            {
                current.reason = candidate.reason;
            }
            let current_props = current.props.as_ref().map(ToString::to_string);
            let candidate_props = candidate.props.as_ref().map(ToString::to_string);
            if candidate_props.is_some()
                && (current_props.is_none() || candidate_props < current_props)
            {
                current.props = candidate.props;
            }
        }
        None => {
            edges.insert(key, candidate);
        }
    }
}

fn push_reason(reasons: &mut Vec<TraversalReason>, reason: TraversalReason) {
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

pub(crate) async fn flow_downstream<S: GraphStore + ?Sized>(
    store: &S,
    entry: &NodeId,
    filter: &FlowFilter,
) -> Result<FlowPage> {
    flow_downstream_with_limits(store, entry, filter, TraversalLimits::PRODUCTION).await
}

async fn flow_downstream_with_limits<S: TraversalSource + ?Sized>(
    store: &S,
    entry: &NodeId,
    filter: &FlowFilter,
    limits: TraversalLimits,
) -> Result<FlowPage> {
    let limit = filter.effective_limit();
    let end = filter
        .offset
        .checked_add(limit)
        .ok_or_else(|| GraphStoreError::InvalidInput("trace offset + limit overflowed".into()))?;
    if end > limits.visible_window {
        return Err(GraphStoreError::InvalidInput(format!(
            "trace result window {end} exceeds {}",
            limits.visible_window
        )));
    }

    let root = store
        .get_node(entry)
        .await?
        .ok_or_else(|| GraphStoreError::NotFound(entry.to_string()))?;
    let max_depth = filter.max_depth.clamp(1, 10);
    let mut traversal = TraversalStats {
        visited_nodes: 1,
        ..TraversalStats::default()
    };
    let mut reached = HashMap::<NodeId, ReachedNode>::new();
    reached.insert(
        entry.clone(),
        ReachedNode {
            node: root,
            depth: 0,
            parent: None,
            via: None,
            is_accessor: false,
        },
    );
    let mut frontier = vec![entry.clone()];

    for depth in 1..=max_depth {
        if frontier.is_empty() || traversal.truncated {
            break;
        }
        sort_frontier(&mut frontier, &reached);
        let transitions = expand_layer(store, &frontier, false, &mut traversal, limits).await?;
        let mut next = Vec::new();
        for transition in transitions {
            if reached.contains_key(&transition.target.id) {
                continue;
            }
            if reached.len() >= limits.node_budget {
                traversal.truncated = true;
                break;
            }
            let target_id = transition.target.id.clone();
            reached.insert(
                target_id.clone(),
                ReachedNode {
                    node: transition.target.clone(),
                    depth,
                    parent: Some(transition.source.clone()),
                    is_accessor: transition.target_is_accessor,
                    via: Some(transition),
                },
            );
            next.push(target_id);
        }
        traversal.visited_nodes = reached.len();
        frontier = next;
    }

    let mut visible: Vec<ReachedNode> = reached
        .into_values()
        .filter(|item| {
            item.depth == 0
                || (!filter.exclude_kinds.contains(&item.node.kind)
                    && !(filter.exclude_accessors && item.is_accessor))
        })
        .collect();
    visible.sort_by(reached_cmp);

    let has_more = visible.len() > end;
    let page = if filter.offset >= visible.len() {
        Vec::new()
    } else {
        visible
            .drain(filter.offset..end.min(visible.len()))
            .collect::<Vec<_>>()
    };
    let mut hops: Vec<FlowHop> = page.into_iter().map(flow_hop).collect();
    annotate_interceptions(store, &mut hops).await?;
    Ok(FlowPage {
        hops,
        has_more,
        traversal,
    })
}

pub(crate) async fn paths_between<S: GraphStore + ?Sized>(
    store: &S,
    from: &NodeId,
    to: &NodeId,
    filter: &PathFilter,
) -> Result<PathPage> {
    paths_between_with_limits(store, from, to, filter, TraversalLimits::PRODUCTION).await
}

async fn paths_between_with_limits<S: TraversalSource + ?Sized>(
    store: &S,
    from: &NodeId,
    to: &NodeId,
    filter: &PathFilter,
    limits: TraversalLimits,
) -> Result<PathPage> {
    let from_node = store
        .get_node(from)
        .await?
        .ok_or_else(|| GraphStoreError::NotFound(from.to_string()))?;
    let target = store
        .get_node(to)
        .await?
        .ok_or_else(|| GraphStoreError::NotFound(to.to_string()))?;
    if filter.access != PathAccess::Any && target.kind != NodeKind::DbTable {
        return Err(GraphStoreError::InvalidInput(
            "read/write access filters require a DbTable target".into(),
        ));
    }

    let max_depth = filter.max_depth.clamp(1, 12);
    let max_paths = filter.max_paths.clamp(1, 10);
    let mut traversal = TraversalStats {
        visited_nodes: 1,
        ..TraversalStats::default()
    };
    if from == to && filter.access == PathAccess::Any {
        return Ok(PathPage {
            paths: vec![PathInfo {
                nodes: vec![from.clone()],
                edges: Vec::new(),
                min_confidence: 1.0,
            }],
            has_more: false,
            traversal,
        });
    }

    let mut node_meta = HashMap::<NodeId, Node>::new();
    node_meta.insert(from.clone(), from_node);
    let mut depth_by_id = HashMap::<NodeId, u32>::new();
    depth_by_id.insert(from.clone(), 0);
    let mut predecessors = HashMap::<NodeId, Vec<Predecessor>>::new();
    let mut frontier = vec![from.clone()];
    let mut target_depth = None;

    for depth in 1..=max_depth {
        if frontier.is_empty() || traversal.truncated || target_depth.is_some() {
            break;
        }
        frontier.sort_by(|a, b| node_id_cmp(a, b, &node_meta));
        let transitions = expand_layer(store, &frontier, true, &mut traversal, limits).await?;
        let mut next = Vec::new();
        for transition in transitions {
            let target_id = transition.target.id.clone();
            if target_id == *to && !final_edge_matches(&transition.kind, filter.access) {
                continue;
            }
            match depth_by_id.get(&target_id).copied() {
                Some(existing) if existing == depth => {
                    push_predecessor(&mut predecessors, &target_id, &transition);
                }
                Some(_) => continue,
                None => {
                    if depth_by_id.len() >= limits.node_budget {
                        traversal.truncated = true;
                        break;
                    }
                    depth_by_id.insert(target_id.clone(), depth);
                    node_meta.insert(target_id.clone(), transition.target.clone());
                    push_predecessor(&mut predecessors, &target_id, &transition);
                    if target_id == *to {
                        target_depth = Some(depth);
                    } else {
                        next.push(target_id);
                    }
                }
            }
        }
        traversal.visited_nodes = depth_by_id.len();
        frontier = next;
    }

    if target_depth.is_none() {
        return Ok(PathPage {
            paths: Vec::new(),
            has_more: false,
            traversal,
        });
    }

    for values in predecessors.values_mut() {
        values.sort_by(|a, b| {
            node_id_cmp(&a.source, &b.source, &node_meta)
                .then_with(|| a.edge.kind.cmp(&b.edge.kind))
                .then_with(|| a.edge.reason.cmp(&b.edge.reason))
        });
        values.dedup_by(|a, b| {
            a.source == b.source
                && a.edge.kind == b.edge.kind
                && a.edge.traversed_reverse == b.edge.traversed_reverse
        });
    }
    let mut paths = Vec::new();
    let mut reverse_nodes = vec![to.clone()];
    let mut reverse_edges = Vec::new();
    reconstruct_paths(
        from,
        to,
        &predecessors,
        &mut reverse_nodes,
        &mut reverse_edges,
        max_paths + 1,
        &mut paths,
    );
    let has_more = paths.len() > max_paths;
    paths.truncate(max_paths);
    Ok(PathPage {
        paths,
        has_more,
        traversal,
    })
}

async fn expand_layer<S: TraversalSource + ?Sized>(
    store: &S,
    frontier: &[NodeId],
    include_data: bool,
    traversal: &mut TraversalStats,
    limits: TraversalLimits,
) -> Result<Vec<ExecutionTransition>> {
    let mut transitions = Vec::new();
    for ids in frontier.chunks(limits.batch_size.max(1)) {
        let remaining = limits.edge_budget.saturating_sub(traversal.expanded_edges);
        if remaining == 0 {
            traversal.truncated = true;
            break;
        }
        let mut batch = store
            .execution_transitions(ids, include_data, remaining.saturating_add(1))
            .await?;
        batch.sort_by(transition_cmp);
        if batch.len() > remaining {
            batch.truncate(remaining);
            traversal.truncated = true;
        }
        traversal.expanded_edges += batch.len();
        transitions.extend(batch);
        if traversal.truncated {
            break;
        }
    }
    transitions.sort_by(transition_cmp);
    transitions.dedup_by(|a, b| {
        a.source == b.source
            && a.target.id == b.target.id
            && a.kind == b.kind
            && a.traversed_reverse == b.traversed_reverse
    });
    Ok(transitions)
}

fn transition_cmp(a: &ExecutionTransition, b: &ExecutionTransition) -> Ordering {
    a.target
        .name
        .cmp(&b.target.name)
        .then_with(|| a.target.id.as_str().cmp(b.target.id.as_str()))
        .then_with(|| a.source.as_str().cmp(b.source.as_str()))
        .then_with(|| a.kind.cmp(&b.kind))
        .then_with(|| a.traversed_reverse.cmp(&b.traversed_reverse))
}

fn reached_cmp(a: &ReachedNode, b: &ReachedNode) -> Ordering {
    a.depth
        .cmp(&b.depth)
        .then_with(|| a.node.name.cmp(&b.node.name))
        .then_with(|| a.node.id.as_str().cmp(b.node.id.as_str()))
}

fn sort_frontier(frontier: &mut [NodeId], reached: &HashMap<NodeId, ReachedNode>) {
    frontier.sort_by(|a, b| {
        let an = reached.get(a).map(|n| n.node.name.as_str()).unwrap_or("");
        let bn = reached.get(b).map(|n| n.node.name.as_str()).unwrap_or("");
        an.cmp(bn).then_with(|| a.as_str().cmp(b.as_str()))
    });
}

fn node_id_cmp(a: &NodeId, b: &NodeId, nodes: &HashMap<NodeId, Node>) -> Ordering {
    let an = nodes.get(a).map(|n| n.name.as_str()).unwrap_or("");
    let bn = nodes.get(b).map(|n| n.name.as_str()).unwrap_or("");
    an.cmp(bn).then_with(|| a.as_str().cmp(b.as_str()))
}

fn flow_hop(item: ReachedNode) -> FlowHop {
    let via = item.via.map(|transition| FlowEdge {
        kind: transition.kind,
        call_sites: transition.call_sites,
    });
    FlowHop {
        node: FlowNode {
            id: item.node.id,
            kind: item.node.kind,
            name: item.node.name,
            qualified_name: item.node.qualified_name,
            file: item.node.file,
            depth: item.depth,
            parent_id: item.parent,
            intercepted_by: Vec::new(),
        },
        via,
    }
}

async fn annotate_interceptions<S: TraversalSource + ?Sized>(
    store: &S,
    hops: &mut [FlowHop],
) -> Result<()> {
    let ids: Vec<NodeId> = hops
        .iter()
        .filter(|hop| matches!(hop.node.kind, NodeKind::Method | NodeKind::Function))
        .map(|hop| hop.node.id.clone())
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let interceptions = store.interceptions_for_methods(&ids).await?;
    let mut by_target = HashMap::<NodeId, Vec<InterceptingAdvice>>::new();
    for interception in interceptions {
        by_target
            .entry(interception.target)
            .or_default()
            .push(interception.advice);
    }
    for values in by_target.values_mut() {
        values.sort_by(|a, b| {
            a.advice
                .as_str()
                .cmp(b.advice.as_str())
                .then_with(|| a.advice_kind.cmp(&b.advice_kind))
        });
    }
    for hop in hops {
        if let Some(values) = by_target.remove(&hop.node.id) {
            hop.node.intercepted_by = values;
        }
    }
    Ok(())
}

fn final_edge_matches(kind: &str, access: PathAccess) -> bool {
    match access {
        PathAccess::Any => true,
        PathAccess::Read => kind == "READS_TABLE",
        PathAccess::Write => kind == "WRITES_TABLE",
    }
}

fn push_predecessor(
    predecessors: &mut HashMap<NodeId, Vec<Predecessor>>,
    target: &NodeId,
    transition: &ExecutionTransition,
) {
    predecessors
        .entry(target.clone())
        .or_default()
        .push(Predecessor {
            source: transition.source.clone(),
            edge: PathEdge {
                kind: transition.kind.clone(),
                confidence: transition.confidence,
                reason: transition.reason.clone(),
                traversed_reverse: transition.traversed_reverse,
            },
        });
}

#[allow(clippy::too_many_arguments)]
fn reconstruct_paths(
    root: &NodeId,
    current: &NodeId,
    predecessors: &HashMap<NodeId, Vec<Predecessor>>,
    reverse_nodes: &mut Vec<NodeId>,
    reverse_edges: &mut Vec<PathEdge>,
    limit: usize,
    paths: &mut Vec<PathInfo>,
) {
    if paths.len() >= limit {
        return;
    }
    if current == root {
        let nodes = reverse_nodes.iter().rev().cloned().collect();
        let edges: Vec<PathEdge> = reverse_edges.iter().rev().cloned().collect();
        let min_confidence = edges
            .iter()
            .map(|edge| edge.confidence)
            .fold(1.0f32, f32::min);
        paths.push(PathInfo {
            nodes,
            edges,
            min_confidence,
        });
        return;
    }
    let Some(values) = predecessors.get(current) else {
        return;
    };
    for predecessor in values {
        reverse_nodes.push(predecessor.source.clone());
        reverse_edges.push(predecessor.edge.clone());
        reconstruct_paths(
            root,
            &predecessor.source,
            predecessors,
            reverse_nodes,
            reverse_edges,
            limit,
            paths,
        );
        reverse_edges.pop();
        reverse_nodes.pop();
        if paths.len() >= limit {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use cih_core::{Node, NodeId, NodeKind};

    use super::{
        final_edge_matches, flow_downstream_with_limits, paths_between_with_limits,
        TraversalLimits, TraversalSource,
    };
    use crate::{
        ExecutionTransition, FlowFilter, Interception, PathAccess, PathFilter, Result,
        TRAVERSAL_EDGE_BUDGET, TRAVERSAL_NODE_BUDGET,
    };

    const ROOT: &str = "Method:test.Root#run/0";

    #[derive(Clone, Copy)]
    enum FanoutShape {
        UniqueTargets,
        DuplicateTarget,
    }

    struct BudgetSource {
        fanout: usize,
        shape: FanoutShape,
    }

    #[async_trait::async_trait]
    impl TraversalSource for BudgetSource {
        async fn get_node(&self, id: &NodeId) -> Result<Option<Node>> {
            Ok(Some(node(id.clone(), id.as_str())))
        }

        async fn execution_transitions(
            &self,
            ids: &[NodeId],
            _include_data: bool,
            limit: usize,
        ) -> Result<Vec<ExecutionTransition>> {
            if !ids.iter().any(|id| id.as_str() == ROOT) {
                return Ok(Vec::new());
            }
            Ok((0..self.fanout.min(limit))
                .map(|index| {
                    let (target_id, target_name) = match self.shape {
                        FanoutShape::UniqueTargets => (
                            NodeId::new(format!("Method:test.Target#{index:05}/0")),
                            format!("target-{index:05}"),
                        ),
                        FanoutShape::DuplicateTarget => {
                            (NodeId::new("Method:test.Decoy#run/0"), "decoy".to_string())
                        }
                    };
                    ExecutionTransition {
                        source: NodeId::new(ROOT),
                        target: node(target_id, &target_name),
                        kind: "CALLS".to_string(),
                        confidence: 1.0,
                        reason: "budget fixture".to_string(),
                        call_sites: Vec::new(),
                        traversed_reverse: false,
                        target_is_accessor: false,
                    }
                })
                .collect())
        }

        async fn interceptions_for_methods(&self, _ids: &[NodeId]) -> Result<Vec<Interception>> {
            Ok(Vec::new())
        }
    }

    fn node(id: NodeId, name: &str) -> Node {
        Node {
            id,
            kind: NodeKind::Method,
            name: name.to_string(),
            qualified_name: None,
            file: "src/Test.java".to_string(),
            range: Default::default(),
            props: None,
        }
    }

    #[test]
    fn access_filter_only_accepts_matching_table_edge() {
        assert!(final_edge_matches("CALLS", PathAccess::Any));
        assert!(final_edge_matches("READS_TABLE", PathAccess::Read));
        assert!(!final_edge_matches("WRITES_TABLE", PathAccess::Read));
        assert!(final_edge_matches("WRITES_TABLE", PathAccess::Write));
        assert!(!final_edge_matches("READS_TABLE", PathAccess::Write));
    }

    #[tokio::test]
    async fn shared_bfs_enforces_the_ten_thousand_node_budget() {
        assert_eq!(TraversalLimits::PRODUCTION.node_budget, 10_000);
        let source = BudgetSource {
            // The root counts toward the budget, so the final transition cannot
            // be admitted and must mark the otherwise valid walk as truncated.
            fanout: TRAVERSAL_NODE_BUDGET,
            shape: FanoutShape::UniqueTargets,
        };
        let page = flow_downstream_with_limits(
            &source,
            &NodeId::new(ROOT),
            &FlowFilter {
                max_depth: 1,
                limit: 1,
                ..FlowFilter::default()
            },
            TraversalLimits::PRODUCTION,
        )
        .await
        .expect("budgeted BFS should return a partial page");

        assert_eq!(page.traversal.visited_nodes, TRAVERSAL_NODE_BUDGET);
        assert_eq!(page.traversal.expanded_edges, TRAVERSAL_NODE_BUDGET);
        assert!(page.traversal.truncated);
    }

    #[tokio::test]
    async fn shared_path_bfs_enforces_fifty_thousand_edges_as_inconclusive() {
        assert_eq!(TraversalLimits::PRODUCTION.edge_budget, 50_000);
        let source = BudgetSource {
            fanout: TRAVERSAL_EDGE_BUDGET + 1,
            // Duplicate logical rows isolate the edge budget from the node
            // budget: all rows expand, then canonicalization keeps one decoy.
            shape: FanoutShape::DuplicateTarget,
        };
        let page = paths_between_with_limits(
            &source,
            &NodeId::new(ROOT),
            &NodeId::new("Method:test.Unreachable#run/0"),
            &PathFilter {
                max_depth: 2,
                max_paths: 1,
                access: PathAccess::Any,
            },
            TraversalLimits::PRODUCTION,
        )
        .await
        .expect("budget exhaustion should be reported, not raised as a backend error");

        assert_eq!(page.traversal.expanded_edges, TRAVERSAL_EDGE_BUDGET);
        assert!(page.paths.is_empty());
        assert!(
            page.traversal.truncated,
            "an empty budget-truncated path result is inconclusive, not unreachable"
        );
    }
}
