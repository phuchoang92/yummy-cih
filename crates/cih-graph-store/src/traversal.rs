//! Backend-neutral execution traversal.
//!
//! Adapters expose deterministic, bounded one-hop transitions. This module
//! owns depth semantics, cycle handling, filtering, pagination, path
//! reconstruction, and resource budgets so both graph backends behave alike.

use std::cmp::Ordering;
use std::collections::HashMap;

use cih_core::{Node, NodeId, NodeKind};

use crate::{
    ExecutionTransition, FlowEdge, FlowFilter, FlowHop, FlowNode, FlowPage, GraphStore,
    GraphStoreError, InterceptingAdvice, PathAccess, PathEdge, PathFilter, PathInfo, PathPage,
    Result, TraversalStats, EXECUTION_BATCH_SIZE, FLOW_VISIBLE_WINDOW, TRAVERSAL_EDGE_BUDGET,
    TRAVERSAL_NODE_BUDGET,
};

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

pub(crate) async fn flow_downstream<S: GraphStore + ?Sized>(
    store: &S,
    entry: &NodeId,
    filter: &FlowFilter,
) -> Result<FlowPage> {
    let limit = filter.effective_limit();
    let end = filter
        .offset
        .checked_add(limit)
        .ok_or_else(|| GraphStoreError::InvalidInput("trace offset + limit overflowed".into()))?;
    if end > FLOW_VISIBLE_WINDOW {
        return Err(GraphStoreError::InvalidInput(format!(
            "trace result window {end} exceeds {FLOW_VISIBLE_WINDOW}"
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
        let transitions = expand_layer(store, &frontier, false, &mut traversal).await?;
        let mut next = Vec::new();
        for transition in transitions {
            if reached.contains_key(&transition.target.id) {
                continue;
            }
            if reached.len() >= TRAVERSAL_NODE_BUDGET {
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
        let transitions = expand_layer(store, &frontier, true, &mut traversal).await?;
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
                    if depth_by_id.len() >= TRAVERSAL_NODE_BUDGET {
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

async fn expand_layer<S: GraphStore + ?Sized>(
    store: &S,
    frontier: &[NodeId],
    include_data: bool,
    traversal: &mut TraversalStats,
) -> Result<Vec<ExecutionTransition>> {
    let mut transitions = Vec::new();
    for ids in frontier.chunks(EXECUTION_BATCH_SIZE) {
        let remaining = TRAVERSAL_EDGE_BUDGET.saturating_sub(traversal.expanded_edges);
        if remaining == 0 {
            traversal.truncated = true;
            break;
        }
        let mut batch = store
            .execution_transitions(ids, include_data, remaining + 1)
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

async fn annotate_interceptions<S: GraphStore + ?Sized>(
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
    use super::final_edge_matches;
    use crate::PathAccess;

    #[test]
    fn access_filter_only_accepts_matching_table_edge() {
        assert!(final_edge_matches("CALLS", PathAccess::Any));
        assert!(final_edge_matches("READS_TABLE", PathAccess::Read));
        assert!(!final_edge_matches("WRITES_TABLE", PathAccess::Read));
        assert!(final_edge_matches("WRITES_TABLE", PathAccess::Write));
        assert!(!final_edge_matches("READS_TABLE", PathAccess::Write));
    }
}
