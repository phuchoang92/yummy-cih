use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use cih_core::NodeId;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use petgraph::Direction;

use crate::ProcessConfig;

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
    let mut all_traces = Vec::new();

    for (entry_id, _) in entry_points {
        let Some(&entry_idx) = index_by_id.get(entry_id) else {
            continue;
        };
        let mut traces_for_entry = Vec::new();
        let mut queue = VecDeque::new();
        queue.push_back((entry_idx, vec![entry_idx]));

        while let Some((cur, path)) = queue.pop_front() {
            let mut callees: Vec<NodeIndex> = digraph
                .edges_directed(cur, Direction::Outgoing)
                .filter(|edge| *edge.weight() >= cfg.min_trace_confidence)
                .map(|edge| edge.target())
                .filter(|next| !path.contains(next))
                .collect();
            callees.sort_by(|a, b| digraph[*a].as_str().cmp(digraph[*b].as_str()));
            callees.dedup();
            callees.truncate(cfg.max_branching);

            if callees.is_empty() || path.len() >= cfg.max_trace_depth {
                if path.len() >= cfg.min_steps {
                    traces_for_entry.push(path);
                }
                continue;
            }

            for next in callees {
                let mut next_path = path.clone();
                next_path.push(next);
                queue.push_back((next, next_path));
            }
        }

        traces_for_entry.sort_by(|a, b| {
            b.len()
                .cmp(&a.len())
                .then_with(|| encode_trace(a, digraph).cmp(&encode_trace(b, digraph)))
        });
        traces_for_entry.truncate(cfg.max_branching * 3);
        all_traces.extend(traces_for_entry);
    }

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

pub(crate) fn deduplicate_traces(
    mut traces: Vec<Vec<NodeIndex>>,
    digraph: &DiGraph<NodeId, f32>,
) -> Vec<Vec<NodeIndex>> {
    traces.sort_by(|a, b| {
        b.len()
            .cmp(&a.len())
            .then_with(|| encode_trace(a, digraph).cmp(&encode_trace(b, digraph)))
    });

    let mut retained: Vec<(String, Vec<NodeIndex>)> = Vec::new();
    for trace in traces {
        let encoded = encode_trace(&trace, digraph);
        if retained
            .iter()
            .any(|(kept, _)| kept.contains(encoded.as_str()))
        {
            continue;
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
