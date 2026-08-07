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
    let forward = combined_edges(vec![a.clone(), b.clone()], vec![]);
    let backward = combined_edges(vec![b.clone(), a.clone()], vec![]);
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
    let result = combined_edges(vec![low], vec![high]);
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
    let result = combined_edges(vec![first], vec![second]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].reason, "first");
}

fn edge_with_sites(confidence: f32, sites: &[&str]) -> Edge {
    let arr: Vec<serde_json::Value> = sites.iter().map(|s| serde_json::json!(s)).collect();
    Edge {
        src: NodeId::new("A"),
        dst: NodeId::new("B"),
        kind: EdgeKind::Calls,
        confidence,
        reason: String::new(),
        props: Some(serde_json::json!({ "call_sites": arr })),
    }
}

fn call_sites_of(edge: &Edge) -> Vec<String> {
    edge.props.as_ref().unwrap()["call_sites"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn call_sites_accumulate_and_props_survive_confidence_promotion() {
    // Lower-confidence structure edge first, higher-confidence resolved edge second:
    // the winner takes the higher-confidence scalar fields but keeps the call_sites
    // accumulated (in order) from both edges.
    let low = edge_with_sites(0.4, &["s1", "s2"]);
    let high = edge_with_sites(0.9, &["s3"]);
    let result = combined_edges(vec![low], vec![high]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].confidence, 0.9);
    assert_eq!(call_sites_of(&result[0]), vec!["s1", "s2", "s3"]);
}

#[test]
fn merged_call_sites_are_capped_at_twenty() {
    let a = edge_with_sites(0.5, &(0..15).map(|_| "a").collect::<Vec<_>>());
    let b = edge_with_sites(0.5, &(0..15).map(|_| "b").collect::<Vec<_>>());
    let result = combined_edges(vec![a], vec![b]);
    assert_eq!(result.len(), 1);
    assert_eq!(
        call_sites_of(&result[0]).len(),
        20,
        "call_sites capped at 20"
    );
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

    let _ = combined_edges(structure.to_vec(), resolved.to_vec());
    let _ = btreemap_combined_edges(structure, resolved);

    let t0 = std::time::Instant::now();
    for _ in 0..ITERS {
        std::hint::black_box(combined_edges(structure.to_vec(), resolved.to_vec()));
    }
    let hashmap_ms = t0.elapsed().as_millis() / ITERS as u128;

    let t1 = std::time::Instant::now();
    for _ in 0..ITERS {
        std::hint::black_box(btreemap_combined_edges(structure, resolved));
    }
    let btreemap_ms = t1.elapsed().as_millis() / ITERS as u128;

    let hm = combined_edges(structure.to_vec(), resolved.to_vec());
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
