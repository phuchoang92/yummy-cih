//! Gap 1: Transitive loop depth propagation.
//!
//! Implements the additive DFS formula from CBM `pass_complexity.c:102`:
//!   `tld(id) = loop_depth(id) + max_over_callees(tld(callee))`
//!
//! Back-edges (detected via in-flight set) set `is_recursive = true` on the
//! source node and return 0 to avoid infinite inflation.
//!
//! Values are capped at 20 to prevent runaway inflation through stdlib loops.
//! Results are written into `Node.props["transitiveLoopDepth"]` and
//! `Node.props["isRecursive"]`.

use std::collections::{HashMap, HashSet};

use cih_core::{Edge, EdgeKind, Node};

const TLD_CAP: u8 = 20;

/// Propagate transitive loop depths along CALLS edges.
/// Mutates `Node.props["transitiveLoopDepth"]` (u8) and `Node.props["isRecursive"]` (bool).
pub fn propagate_loop_depths(nodes: &mut [Node], edges: &[Edge]) {
    // Build adjacency: src_id → [dst_id] for CALLS edges.
    let mut callees: HashMap<String, Vec<String>> = HashMap::new();
    for edge in edges {
        if edge.kind == EdgeKind::Calls {
            callees
                .entry(edge.src.as_str().to_string())
                .or_default()
                .push(edge.dst.as_str().to_string());
        }
    }

    // Build a map of node_id → index for mutation.
    let mut id_to_idx: HashMap<String, usize> = HashMap::new();
    for (i, n) in nodes.iter().enumerate() {
        id_to_idx.insert(n.id.as_str().to_string(), i);
    }

    // Memoize computed tld values to avoid re-traversal.
    let mut memo: HashMap<String, u8> = HashMap::new();
    // Track nodes currently in the DFS stack (for cycle detection).
    let mut in_flight: HashSet<String> = HashSet::new();
    // Track nodes found to be recursive.
    let mut recursive_ids: HashSet<String> = HashSet::new();

    let node_ids: Vec<String> = nodes.iter().map(|n| n.id.as_str().to_string()).collect();

    for id in &node_ids {
        if !memo.contains_key(id.as_str()) {
            dfs(
                id,
                &callees,
                &id_to_idx,
                nodes,
                &mut memo,
                &mut in_flight,
                &mut recursive_ids,
            );
        }
    }

    // Write results back to node props.
    for id in &node_ids {
        if let Some(&tld) = memo.get(id.as_str()) {
            if let Some(&idx) = id_to_idx.get(id.as_str()) {
                let n = &mut nodes[idx];
                let is_recursive = recursive_ids.contains(id.as_str());
                let props = n.props.get_or_insert_with(|| serde_json::json!({}));
                props["transitiveLoopDepth"] = serde_json::Value::from(tld as u64);
                if is_recursive {
                    props["isRecursive"] = serde_json::Value::Bool(true);
                }
            }
        }
    }
}

fn own_loop_depth(id: &str, id_to_idx: &HashMap<String, usize>, nodes: &[Node]) -> u8 {
    let Some(&idx) = id_to_idx.get(id) else {
        return 0;
    };
    let n = &nodes[idx];
    n.props
        .as_ref()
        .and_then(|p| p.get("loopDepth"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u8
}

// Explicit-stack DFS: CALLS chains grow with repository size (a million-node
// graph can hold call chains far deeper than the thread stack), so the
// traversal must not recurse. Visit order, memoization, and back-edge
// handling are identical to the recursive formulation.
fn dfs<'a>(
    root: &'a str,
    callees: &'a HashMap<String, Vec<String>>,
    id_to_idx: &HashMap<String, usize>,
    nodes: &[Node],
    memo: &mut HashMap<String, u8>,
    in_flight: &mut HashSet<String>,
    recursive_ids: &mut HashSet<String>,
) {
    struct Frame<'a> {
        id: &'a str,
        own_ld: u8,
        next_callee: usize,
        max_callee_tld: u8,
    }

    in_flight.insert(root.to_string());
    let mut stack = vec![Frame {
        id: root,
        own_ld: own_loop_depth(root, id_to_idx, nodes),
        next_callee: 0,
        max_callee_tld: 0,
    }];

    while let Some(top) = stack.last_mut() {
        let id = top.id;
        let dsts = callees.get(id).map(Vec::as_slice).unwrap_or(&[]);
        if let Some(dst) = dsts.get(top.next_callee) {
            top.next_callee += 1;
            let dst = dst.as_str();
            if let Some(&cached) = memo.get(dst) {
                top.max_callee_tld = top.max_callee_tld.max(cached);
            } else if in_flight.contains(dst) {
                // Back-edge: cycle detected; contributes 0.
                recursive_ids.insert(dst.to_string());
            } else {
                in_flight.insert(dst.to_string());
                stack.push(Frame {
                    id: dst,
                    own_ld: own_loop_depth(dst, id_to_idx, nodes),
                    next_callee: 0,
                    max_callee_tld: 0,
                });
            }
        } else {
            let tld = (top.own_ld as u16 + top.max_callee_tld as u16).min(TLD_CAP as u16) as u8;
            in_flight.remove(id);
            memo.insert(id.to_string(), tld);
            stack.pop();
            if let Some(parent) = stack.last_mut() {
                parent.max_callee_tld = parent.max_callee_tld.max(tld);
            }
        }
    }
}
