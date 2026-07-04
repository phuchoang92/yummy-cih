use std::collections::HashMap;

use cih_core::Edge;

pub(super) fn combined_edges(structure: &[Edge], resolved: &[Edge]) -> Vec<Edge> {
    let mut map: HashMap<(String, String, &'static str), Edge> =
        HashMap::with_capacity(structure.len() + resolved.len());
    for edge in structure.iter().chain(resolved.iter()) {
        let key = (
            edge.src.as_str().to_string(),
            edge.dst.as_str().to_string(),
            edge.kind.cypher_label(),
        );
        match map.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let winner = slot.get_mut();
                merge_call_sites(winner, edge);
                if edge.confidence > winner.confidence {
                    let merged_props = winner.props.take();
                    *winner = edge.clone();
                    winner.props = merged_props;
                }
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(edge.clone());
            }
        }
    }
    let mut result: Vec<Edge> = map.into_values().collect();
    result.sort_unstable_by(|a, b| {
        a.src
            .as_str()
            .cmp(b.src.as_str())
            .then_with(|| a.dst.as_str().cmp(b.dst.as_str()))
            .then_with(|| a.kind.cypher_label().cmp(b.kind.cypher_label()))
    });
    result
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

