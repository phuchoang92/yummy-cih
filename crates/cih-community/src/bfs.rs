use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

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
    let mut all_traces = Vec::new();

    for (entry_id, _) in entry_points {
        let Some(&entry_idx) = index_by_id.get(entry_id) else {
            continue;
        };
        let mut traces_for_entry = Vec::new();
        let mut states = Vec::new();
        let mut queue = VecDeque::new();
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
            callees.dedup();
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
                let next_state = TraceState {
                    node: next,
                    parent: Some(state_idx),
                    depth: state.depth + 1,
                };
                states.push(next_state);
                queue.push_back(states.len() - 1);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> NodeId {
        NodeId::new(format!("Method:{name}#{}/0", name.to_ascii_lowercase()))
    }

    fn cfg() -> ProcessConfig {
        ProcessConfig {
            max_trace_depth: 10,
            max_branching: 4,
            max_processes: 100,
            min_steps: 2,
            min_trace_confidence: 0.5,
            max_states_per_entry: 50_000,
        }
    }

    fn graph_with_nodes(names: &[&str]) -> (DiGraph<NodeId, f32>, Vec<NodeIndex>) {
        let mut graph = DiGraph::<NodeId, f32>::new();
        let indexes = names.iter().map(|name| graph.add_node(id(name))).collect();
        (graph, indexes)
    }

    fn entry(graph: &DiGraph<NodeId, f32>, idx: NodeIndex) -> Vec<(NodeId, f64)> {
        vec![(graph[idx].clone(), 1.0)]
    }

    #[test]
    fn parent_pointer_bfs_prevents_cycles() {
        let (mut graph, nodes) = graph_with_nodes(&["A", "B", "C"]);
        graph.add_edge(nodes[0], nodes[1], 1.0);
        graph.add_edge(nodes[1], nodes[2], 1.0);
        graph.add_edge(nodes[2], nodes[0], 1.0);

        let traces = trace_process_paths(&graph, &entry(&graph, nodes[0]), &HashMap::new(), &cfg());

        assert_eq!(traces.len(), 1);
        assert_eq!(
            encode_trace(&traces[0], &graph),
            "Method:A#a/0->Method:B#b/0->Method:C#c/0"
        );
    }

    #[test]
    fn parent_pointer_bfs_respects_max_branching() {
        let (mut graph, nodes) = graph_with_nodes(&["A", "B", "C", "D"]);
        graph.add_edge(nodes[0], nodes[3], 1.0);
        graph.add_edge(nodes[0], nodes[2], 1.0);
        graph.add_edge(nodes[0], nodes[1], 1.0);
        let mut cfg = cfg();
        cfg.max_branching = 2;

        let traces = trace_process_paths(&graph, &entry(&graph, nodes[0]), &HashMap::new(), &cfg);
        let encoded: Vec<_> = traces
            .iter()
            .map(|trace| encode_trace(trace, &graph))
            .collect();

        assert_eq!(encoded.len(), 2);
        assert!(encoded.iter().any(|s| s.ends_with("Method:B#b/0")));
        assert!(encoded.iter().any(|s| s.ends_with("Method:C#c/0")));
        assert!(!encoded.iter().any(|s| s.ends_with("Method:D#d/0")));
    }

    #[test]
    fn parent_pointer_bfs_respects_max_trace_depth() {
        let (mut graph, nodes) = graph_with_nodes(&["A", "B", "C", "D"]);
        graph.add_edge(nodes[0], nodes[1], 1.0);
        graph.add_edge(nodes[1], nodes[2], 1.0);
        graph.add_edge(nodes[2], nodes[3], 1.0);
        let mut cfg = cfg();
        cfg.max_trace_depth = 3;

        let traces = trace_process_paths(&graph, &entry(&graph, nodes[0]), &HashMap::new(), &cfg);

        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].len(), 3);
        assert_eq!(
            encode_trace(&traces[0], &graph),
            "Method:A#a/0->Method:B#b/0->Method:C#c/0"
        );
    }

    #[test]
    fn parent_pointer_bfs_respects_max_states_per_entry() {
        let (mut graph, nodes) = graph_with_nodes(&["A", "B", "C", "D"]);
        graph.add_edge(nodes[0], nodes[1], 1.0);
        graph.add_edge(nodes[0], nodes[2], 1.0);
        graph.add_edge(nodes[0], nodes[3], 1.0);
        let mut cfg = cfg();
        cfg.max_states_per_entry = 3;

        let traces = trace_process_paths(&graph, &entry(&graph, nodes[0]), &HashMap::new(), &cfg);
        let encoded: Vec<_> = traces
            .iter()
            .map(|trace| encode_trace(trace, &graph))
            .collect();

        assert_eq!(encoded.len(), 2);
        assert!(encoded.iter().any(|s| s.ends_with("Method:B#b/0")));
        assert!(encoded.iter().any(|s| s.ends_with("Method:C#c/0")));
        assert!(!encoded.iter().any(|s| s.ends_with("Method:D#d/0")));
    }

    #[test]
    fn parent_pointer_bfs_is_deterministic() {
        let (mut graph, nodes) = graph_with_nodes(&["A", "B", "C", "D"]);
        graph.add_edge(nodes[0], nodes[3], 1.0);
        graph.add_edge(nodes[0], nodes[1], 1.0);
        graph.add_edge(nodes[0], nodes[2], 1.0);

        let first = trace_process_paths(&graph, &entry(&graph, nodes[0]), &HashMap::new(), &cfg());
        let second = trace_process_paths(&graph, &entry(&graph, nodes[0]), &HashMap::new(), &cfg());

        assert_eq!(
            first
                .iter()
                .map(|trace| encode_trace(trace, &graph))
                .collect::<Vec<_>>(),
            second
                .iter()
                .map(|trace| encode_trace(trace, &graph))
                .collect::<Vec<_>>()
        );
    }
}
