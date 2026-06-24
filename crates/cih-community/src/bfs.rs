use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

#[cfg(feature = "rayon")]
use rayon::prelude::*;

use cih_core::NodeId;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use petgraph::Direction;

use crate::ProcessConfig;

#[derive(Clone, Copy, Debug)]
struct TraceState {
    node: NodeIndex,
    parent: Option<usize>,
    depth: usize,
}

pub fn trace_process_paths(
    digraph: &DiGraph<NodeId, f32>,
    entry_points: &[(NodeId, f64)],
    memberships: &HashMap<NodeId, NodeId>,
    cfg: &ProcessConfig,
) -> Vec<Vec<NodeIndex>> {
    let index_by_id: HashMap<NodeId, NodeIndex> = digraph
        .node_indices()
        .map(|idx| (digraph[idx].clone(), idx))
        .collect();

    // Each entry point's BFS is fully independent — parallelise across CPU cores.
    #[cfg(feature = "rayon")]
    let all_traces: Vec<Vec<NodeIndex>> = entry_points
        .par_iter()
        .flat_map(|(entry_id, _)| trace_from_entry(entry_id, &index_by_id, digraph, cfg))
        .collect();

    #[cfg(not(feature = "rayon"))]
    let all_traces: Vec<Vec<NodeIndex>> = entry_points
        .iter()
        .flat_map(|(entry_id, _)| trace_from_entry(entry_id, &index_by_id, digraph, cfg))
        .collect();

    let mut traces = deduplicate_traces(all_traces, digraph);
    traces.sort_by(|a, b| {
        let a_cross = trace_crosses_communities(a, digraph, memberships);
        let b_cross = trace_crosses_communities(b, digraph, memberships);
        b.len()
            .cmp(&a.len())
            .then_with(|| b_cross.cmp(&a_cross))
            .then_with(|| encode_trace(a, digraph).cmp(&encode_trace(b, digraph)))
    });
    traces.truncate(cfg.max_processes);
    traces
}

/// BFS from a single entry point; returns all accepted traces.
fn trace_from_entry(
    entry_id: &NodeId,
    index_by_id: &HashMap<NodeId, NodeIndex>,
    digraph: &DiGraph<NodeId, f32>,
    cfg: &ProcessConfig,
) -> Vec<Vec<NodeIndex>> {
    let Some(&entry_idx) = index_by_id.get(entry_id) else {
        return vec![];
    };
    let mut traces_for_entry: Vec<Vec<NodeIndex>> = Vec::new();
    let mut states: Vec<TraceState> = Vec::new();
    let mut queue: VecDeque<usize> = VecDeque::new();
    states.push(TraceState {
        node: entry_idx,
        parent: None,
        depth: 1,
    });
    queue.push_back(0usize);
    let max_states = cfg.max_states_per_entry.max(1);

    while let Some(state_idx) = queue.pop_front() {
        let state = states[state_idx];
        let mut callees: Vec<NodeIndex> = digraph
            .edges_directed(state.node, Direction::Outgoing)
            .filter(|edge| *edge.weight() >= cfg.min_trace_confidence)
            .map(|edge| edge.target())
            .filter(|next| !contains_ancestor(&states, state_idx, *next))
            .collect();
        callees.sort_by(|a, b| digraph[*a].as_str().cmp(digraph[*b].as_str()));
        callees.dedup_by_key(|n| *n);
        callees.truncate(cfg.max_branching);

        if callees.is_empty() || state.depth >= cfg.max_trace_depth {
            if state.depth >= cfg.min_steps {
                traces_for_entry.push(reconstruct_path(&states, state_idx));
            }
            continue;
        }
        if states.len() >= max_states {
            if state.depth >= cfg.min_steps {
                traces_for_entry.push(reconstruct_path(&states, state_idx));
            }
            continue;
        }
        for next in callees {
            if states.len() >= max_states {
                break;
            }
            states.push(TraceState {
                node: next,
                parent: Some(state_idx),
                depth: state.depth + 1,
            });
            queue.push_back(states.len() - 1);
        }
    }

    traces_for_entry.sort_by(|a, b| {
        b.len()
            .cmp(&a.len())
            .then_with(|| encode_trace(a, digraph).cmp(&encode_trace(b, digraph)))
    });
    traces_for_entry.truncate(cfg.max_branching * 3);
    traces_for_entry
}

fn contains_ancestor(states: &[TraceState], mut state_idx: usize, next: NodeIndex) -> bool {
    loop {
        let state = states[state_idx];
        if state.node == next {
            return true;
        }
        match state.parent {
            Some(parent) => state_idx = parent,
            None => return false,
        }
    }
}

fn reconstruct_path(states: &[TraceState], mut state_idx: usize) -> Vec<NodeIndex> {
    let mut path = Vec::with_capacity(states[state_idx].depth);
    loop {
        let state = states[state_idx];
        path.push(state.node);
        match state.parent {
            Some(parent) => state_idx = parent,
            None => {
                path.reverse();
                return path;
            }
        }
    }
}

/// Deduplicate traces: drop any trace that is a contiguous sub-sequence of a
/// longer retained trace, then keep only the longest trace per (entry, terminal) pair.
///
/// Uses a window-set to detect sub-traces in O(N·L²) rather than O(N²·L).
pub(crate) fn deduplicate_traces(
    mut traces: Vec<Vec<NodeIndex>>,
    digraph: &DiGraph<NodeId, f32>,
) -> Vec<Vec<NodeIndex>> {
    traces.sort_by(|a, b| {
        b.len()
            .cmp(&a.len())
            .then_with(|| encode_trace(a, digraph).cmp(&encode_trace(b, digraph)))
    });

    // All contiguous sub-sequences of already-retained traces.  A new trace
    // whose encoding appears in this set is fully covered by a longer one.
    let mut subtrace_windows: HashSet<String> = HashSet::new();
    let mut retained: Vec<(String, Vec<NodeIndex>)> = Vec::new();

    for trace in traces {
        let encoded = encode_trace(&trace, digraph);
        if subtrace_windows.contains(&encoded) {
            continue;
        }
        // Register every window of this trace so shorter sub-traces are caught.
        let segments: Vec<&str> = encoded.split("->").collect();
        for len in 1..=segments.len() {
            for start in 0..=segments.len() - len {
                subtrace_windows.insert(segments[start..start + len].join("->"));
            }
        }
        retained.push((encoded, trace));
    }

    let mut by_endpoint: BTreeMap<(String, String), Vec<NodeIndex>> = BTreeMap::new();
    for (_, trace) in retained {
        let Some(first) = trace.first() else {
            continue;
        };
        let Some(last) = trace.last() else {
            continue;
        };
        let key = (
            digraph[*first].as_str().to_string(),
            digraph[*last].as_str().to_string(),
        );
        match by_endpoint.get(&key) {
            Some(existing)
                if existing.len() > trace.len()
                    || (existing.len() == trace.len()
                        && encode_trace(existing, digraph) <= encode_trace(&trace, digraph)) => {}
            _ => {
                by_endpoint.insert(key, trace);
            }
        }
    }

    by_endpoint.into_values().collect()
}

/// Returns true when every `->` segment of `candidate` appears as a contiguous
/// subsequence of segments in `of`. A plain string `contains` would also match
/// if one node-ID string happened to be a substring of another.
#[cfg(test)]
pub(crate) fn is_subtrace_of(candidate: &str, of: &str) -> bool {
    let c: Vec<&str> = candidate.split("->").collect();
    let o: Vec<&str> = of.split("->").collect();
    if c.len() > o.len() {
        return false;
    }
    o.windows(c.len()).any(|w| w == c.as_slice())
}

fn trace_crosses_communities(
    trace: &[NodeIndex],
    digraph: &DiGraph<NodeId, f32>,
    memberships: &HashMap<NodeId, NodeId>,
) -> bool {
    let mut seen = HashSet::new();
    for idx in trace {
        if let Some(comm) = memberships.get(&digraph[*idx]) {
            seen.insert(comm.as_str().to_string());
        }
    }
    seen.len() > 1
}

fn encode_trace(trace: &[NodeIndex], digraph: &DiGraph<NodeId, f32>) -> String {
    trace
        .iter()
        .map(|idx| digraph[*idx].as_str())
        .collect::<Vec<_>>()
        .join("->")
}

#[cfg(test)]
mod tests;
