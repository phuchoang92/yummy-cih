use cih_core::Edge;

/// Deduplicate structure + resolved edges by `(src, dst, kind)`, keeping the
/// highest-confidence edge and merging `call_sites`, ordered deterministically.
///
/// Takes both inputs **by value** and merges them in place — no per-edge clone,
/// no side `HashMap` (which also allocated two key `String`s per entry), and no
/// second collected Vec. Peak memory is one moved buffer instead of
/// `inputs + cloned map + result`, and the caller's raw edge vectors are freed
/// here rather than lingering through the whole write phase.
pub(super) fn combined_edges(mut structure: Vec<Edge>, resolved: Vec<Edge>) -> Vec<Edge> {
    // `structure` first, then `resolved`, matching the order the previous
    // implementation fed the map (`structure.iter().chain(resolved.iter())`).
    structure.extend(resolved);
    let mut edges = structure;

    // Stable sort groups duplicate keys while preserving structure-before-resolved
    // order (and original order within each), so a run folds in the same sequence
    // the map processed it — required for the stateful call-sites/confidence merge.
    edges.sort_by(|a, b| edge_key(a).cmp(&edge_key(b)));

    // Fold each run of equal keys into its first (retained) element in place.
    // `dedup_by` passes the later element and the retained winner; it always
    // compares subsequent elements against that same evolving winner.
    edges.dedup_by(|later, winner| {
        if edge_key(later) != edge_key(winner) {
            return false;
        }
        merge_call_sites(winner, later);
        if later.confidence > winner.confidence {
            // Promote the higher-confidence edge but keep the accumulated props.
            let merged_props = winner.props.take();
            std::mem::swap(winner, later);
            winner.props = merged_props;
        }
        true
    });

    edges
}

fn edge_key(edge: &Edge) -> (&str, &str, &'static str) {
    (
        edge.src.as_str(),
        edge.dst.as_str(),
        edge.kind.cypher_label(),
    )
}

/// Merge `call_sites` from `incoming` into `winner` (Gap 3). Caps total at 20 per edge.
fn merge_call_sites(winner: &mut Edge, incoming: &Edge) {
    let Some(incoming_props) = &incoming.props else {
        return;
    };
    let Some(incoming_arr) = incoming_props.get("call_sites").and_then(|v| v.as_array()) else {
        return;
    };
    if incoming_arr.is_empty() {
        return;
    }
    let entry = winner
        .props
        .get_or_insert_with(|| serde_json::json!({"call_sites": []}));
    let existing = entry
        .get_mut("call_sites")
        .and_then(|v| v.as_array_mut())
        .expect("call_sites must be an array");
    existing.extend(incoming_arr.iter().cloned());
    existing.truncate(20);
}

#[cfg(test)]
#[path = "merge_tests.rs"]
mod combined_edges_tests;
