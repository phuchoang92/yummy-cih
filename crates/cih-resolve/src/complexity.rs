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

use cih_core::{Edge, EdgeKind, Node, NodeId};

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

fn dfs(
    id: &str,
    callees: &HashMap<String, Vec<String>>,
    id_to_idx: &HashMap<String, usize>,
    nodes: &[Node],
    memo: &mut HashMap<String, u8>,
    in_flight: &mut HashSet<String>,
    recursive_ids: &mut HashSet<String>,
) -> u8 {
    if let Some(&cached) = memo.get(id) {
        return cached;
    }
    // Back-edge: cycle detected.
    if in_flight.contains(id) {
        recursive_ids.insert(id.to_string());
        return 0;
    }

    in_flight.insert(id.to_string());

    let own_ld = own_loop_depth(id, id_to_idx, nodes);

    let max_callee_tld = callees
        .get(id)
        .map(|dsts| {
            dsts.iter()
                .map(|dst| {
                    dfs(
                        dst,
                        callees,
                        id_to_idx,
                        nodes,
                        memo,
                        in_flight,
                        recursive_ids,
                    )
                })
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);

    let tld = (own_ld as u16 + max_callee_tld as u16).min(TLD_CAP as u16) as u8;

    in_flight.remove(id);
    memo.insert(id.to_string(), tld);
    tld
}

/// Convert a raw `NodeId` string reference to a plain `&str` for map lookup.
#[allow(dead_code)]
fn nid_str(id: &NodeId) -> &str {
    id.as_str()
}
