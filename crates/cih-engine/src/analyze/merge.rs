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
mod combined_edges_tests {
    use super::*;
    use cih_core::{EdgeKind, NodeId};

    fn edge(src: &str, dst: &str, kind: EdgeKind, confidence: f32) -> Edge {
        Edge {
            src: NodeId::new(src),
            dst: NodeId::new(dst),
            kind,
            confidence,
            reason: String::new(),
            props: None,
        }
    }

    #[test]
    fn deterministic_order_regardless_of_input_order() {
        let a = edge("A", "B", EdgeKind::Calls, 1.0);
        let b = edge("C", "D", EdgeKind::Calls, 1.0);
        let forward = combined_edges(&[a.clone(), b.clone()], &[]);
        let backward = combined_edges(&[b.clone(), a.clone()], &[]);
        let keys = |v: &[Edge]| {
            v.iter()
                .map(|e| (e.src.as_str().to_string(), e.dst.as_str().to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&forward), keys(&backward));
    }

    #[test]
    fn highest_confidence_wins() {
        let low = edge("A", "B", EdgeKind::Calls, 0.5);
        let high = edge("A", "B", EdgeKind::Calls, 0.9);
        let result = combined_edges(&[low], &[high]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].confidence, 0.9);
    }

    #[test]
    fn equal_confidence_retains_first() {
        let first = Edge {
            src: NodeId::new("A"),
            dst: NodeId::new("B"),
            kind: EdgeKind::Calls,
            confidence: 0.7,
            reason: "first".into(),
            props: None,
        };
        let second = Edge {
            src: NodeId::new("A"),
            dst: NodeId::new("B"),
            kind: EdgeKind::Calls,
            confidence: 0.7,
            reason: "second".into(),
            props: None,
        };
        let result = combined_edges(&[first], &[second]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].reason, "first");
    }

    fn btreemap_combined_edges(structure: &[Edge], resolved: &[Edge]) -> Vec<Edge> {
        let mut map: std::collections::BTreeMap<(String, String, &'static str), Edge> =
            std::collections::BTreeMap::new();
        for edge in structure.iter().chain(resolved.iter()).cloned() {
            let key = (
                edge.src.as_str().to_string(),
                edge.dst.as_str().to_string(),
                edge.kind.cypher_label(),
            );
            match map.entry(key) {
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    if edge.confidence > slot.get().confidence {
                        *slot.get_mut() = edge;
                    }
                }
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(edge);
                }
            }
        }
        map.into_values().collect()
    }

    fn make_edges(n_unique: usize, dup_factor: usize) -> Vec<Edge> {
        let mut v = Vec::with_capacity(n_unique * dup_factor);
        for i in 0..n_unique {
            for d in 0..dup_factor {
                v.push(Edge {
                    src: NodeId::new(format!("com.example.pkg{}.Class{}A", i / 100, i)),
                    dst: NodeId::new(format!("com.example.pkg{}.Class{}B", i / 100, i)),
                    kind: EdgeKind::Calls,
                    confidence: (d as f32) / (dup_factor as f32),
                    reason: String::new(),
                    props: None,
                });
            }
        }
        v
    }

    #[test]
    fn bench_combined_edges() {
        let edges = make_edges(200_000, 10);
        let mid = edges.len() / 2;
        let structure = &edges[..mid];
        let resolved = &edges[mid..];

        const ITERS: u32 = 5;

        let _ = combined_edges(structure, resolved);
        let _ = btreemap_combined_edges(structure, resolved);

        let t0 = std::time::Instant::now();
        for _ in 0..ITERS {
            std::hint::black_box(combined_edges(structure, resolved));
        }
        let hashmap_ms = t0.elapsed().as_millis() / ITERS as u128;

        let t1 = std::time::Instant::now();
        for _ in 0..ITERS {
            std::hint::black_box(btreemap_combined_edges(structure, resolved));
        }
        let btreemap_ms = t1.elapsed().as_millis() / ITERS as u128;

        let hm = combined_edges(structure, resolved);
        let bt = btreemap_combined_edges(structure, resolved);
        assert_eq!(hm.len(), bt.len(), "output length mismatch");
        for (h, b) in hm.iter().zip(bt.iter()) {
            assert_eq!(h.src.as_str(), b.src.as_str(), "src mismatch");
            assert_eq!(h.dst.as_str(), b.dst.as_str(), "dst mismatch");
            assert_eq!(
                h.kind.cypher_label(),
                b.kind.cypher_label(),
                "kind mismatch"
            );
            assert!(
                (h.confidence - b.confidence).abs() < f32::EPSILON,
                "confidence mismatch at {} → {}: {} vs {}",
                h.src.as_str(),
                h.dst.as_str(),
                h.confidence,
                b.confidence
            );
        }

        println!(
            "\ncombined_edges ({} unique, {} total edges, {} iters each):",
            200_000,
            edges.len(),
            ITERS
        );
        println!("  HashMap + sort : {}ms avg", hashmap_ms);
        println!("  BTreeMap       : {}ms avg", btreemap_ms);
        if btreemap_ms > 0 {
            println!(
                "  Speedup        : {:.2}x",
                btreemap_ms as f64 / hashmap_ms as f64
            );
        }
    }
}
