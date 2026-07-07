//! Gap 2: MinHash + LSH near-clone detection.
//!
//! Parameters matching CBM minhash.h:
//!   K=64, threshold=0.95, 32 bands × 2 rows, max 10 SIMILAR_TO edges per node.
//!
//! Only nodes with a `body_fingerprint` stored in props with `leafTokenCount >= MIN_LEAF_TOKENS`
//! are candidates. Pairs are grouped by language provider (no cross-language pairs).
//! Each LSH band of `LSH_ROWS` consecutive MinHash values is hashed to a bucket;
//! candidate pairs in the same bucket get exact Jaccard computed.

use std::collections::HashMap;

use cih_core::{Edge, EdgeKind, Node, NodeId};

const K: usize = 64;
const JACCARD_THRESHOLD: f32 = 0.95;
const LSH_BANDS: usize = 32;
const LSH_ROWS: usize = 2;
const MAX_EDGES_PER_NODE: usize = 10;
const MIN_LEAF_TOKENS: u32 = 30;

struct Candidate {
    id: NodeId,
    provider: String,
    minhash: Vec<u32>,
}

/// Emit SIMILAR_TO edges for near-duplicate method bodies.
/// Returns edges with `confidence = jaccard_score`, capped at `MAX_EDGES_PER_NODE` per source.
pub fn emit_similar_to_edges(nodes: &[Node]) -> Vec<Edge> {
    // Collect owned candidate data from node props.
    let candidates: Vec<Candidate> = nodes
        .iter()
        .filter_map(|n| {
            // Body fingerprint is written into props by the Java parser.
            let fp = n.props.as_ref()?.get("bodyFingerprint")?;
            let provider = fp.get("provider")?.as_str()?.to_string();
            let leaf_count = fp.get("leafTokenCount")?.as_u64()? as u32;
            if leaf_count < MIN_LEAF_TOKENS {
                return None;
            }
            let minhash_arr = fp.get("minhash")?.as_array()?;
            if minhash_arr.len() < K {
                return None;
            }
            let minhash: Vec<u32> = minhash_arr
                .iter()
                .take(K)
                .map(|v| v.as_u64().unwrap_or(0) as u32)
                .collect();
            Some(Candidate {
                id: n.id.clone(),
                provider,
                minhash,
            })
        })
        .collect();

    if candidates.len() < 2 {
        return Vec::new();
    }

    // Group by provider.
    let mut by_provider: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, c) in candidates.iter().enumerate() {
        by_provider.entry(c.provider.as_str()).or_default().push(i);
    }

    // Per-source edge list: candidate_idx → Vec<(dst_idx, jaccard)>.
    let mut edge_map: HashMap<usize, Vec<(usize, f32)>> = HashMap::new();

    for (_provider, indices) in &by_provider {
        if indices.len() < 2 {
            continue;
        }

        // LSH: for each band, map band_hash → list of candidate indices.
        let mut buckets: HashMap<(usize, u64), Vec<usize>> = HashMap::new();
        for &ci in indices.iter() {
            let mh = &candidates[ci].minhash;
            for band in 0..LSH_BANDS {
                let start = band * LSH_ROWS;
                let band_hash = fnv_hash_band(&mh[start..start + LSH_ROWS]);
                buckets.entry((band, band_hash)).or_default().push(ci);
            }
        }

        // For each bucket with ≥2 candidates, compute exact Jaccard.
        let mut checked: std::collections::HashSet<(usize, usize)> =
            std::collections::HashSet::new();
        for bucket in buckets.values() {
            if bucket.len() < 2 {
                continue;
            }
            for (bi, &a) in bucket.iter().enumerate() {
                for &b in &bucket[bi + 1..] {
                    let pair = if a < b { (a, b) } else { (b, a) };
                    if !checked.insert(pair) {
                        continue;
                    }
                    let jaccard =
                        exact_jaccard(&candidates[pair.0].minhash, &candidates[pair.1].minhash);
                    if jaccard >= JACCARD_THRESHOLD {
                        edge_map.entry(pair.0).or_default().push((pair.1, jaccard));
                        edge_map.entry(pair.1).or_default().push((pair.0, jaccard));
                    }
                }
            }
        }
    }

    // Build edges, capping at MAX_EDGES_PER_NODE per source.
    let mut edges = Vec::new();
    for (src_idx, mut neighbors) in edge_map {
        // Sort descending by Jaccard.
        neighbors
            .sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        neighbors.truncate(MAX_EDGES_PER_NODE);
        for (dst_idx, jaccard) in neighbors {
            edges.push(Edge {
                src: candidates[src_idx].id.clone(),
                dst: candidates[dst_idx].id.clone(),
                kind: EdgeKind::SimilarTo,
                confidence: jaccard,
                reason: "minhash-lsh".to_string(),
                props: None,
            });
        }
    }

    edges
}

/// Exact Jaccard from two K-length MinHash vectors: count(a[i] == b[i]) / K.
fn exact_jaccard(a: &[u32], b: &[u32]) -> f32 {
    let matching = a.iter().zip(b.iter()).filter(|(x, y)| x == y).count();
    matching as f32 / K as f32
}

/// FNV-1a hash for a band of MinHash values.
fn fnv_hash_band(band: &[u32]) -> u64 {
    const FNV_OFFSET: u64 = 14695981039346656037;
    const FNV_PRIME: u64 = 1099511628211;
    let mut hash = FNV_OFFSET;
    for &v in band {
        for byte in v.to_le_bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    hash
}
