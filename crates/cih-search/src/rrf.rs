use rustc_hash::{FxHashMap};
use std::collections::HashMap;

use cih_core::{NodeId, NodeKind, Range};
use serde::{Deserialize, Serialize};

/// Standard Reciprocal Rank Fusion smoothing constant; rank-1 contributes 1 / (60 + 1).
pub const RRF_K: f32 = 60.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchHit {
    pub node_id: NodeId,
    pub kind: NodeKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
    pub file: String,
    pub range: Range,
    pub score: f32,
    pub rank: usize,
    pub sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bm25_score: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_score: Option<f32>,
}

impl SearchHit {
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        node_id: NodeId,
        kind: NodeKind,
        name: String,
        qualified_name: Option<String>,
        file: String,
        range: Range,
        source_score: f32,
        source: &str,
    ) -> Self {
        let mut hit = Self {
            node_id,
            kind,
            name,
            qualified_name,
            file,
            range,
            score: source_score,
            rank: 0,
            sources: vec![source.to_string()],
            bm25_score: None,
            semantic_score: None,
        };
        match source {
            "bm25" => hit.bm25_score = Some(source_score),
            "semantic" => hit.semantic_score = Some(source_score),
            _ => {}
        }
        hit
    }
}

pub fn rrf_merge(
    lexical_hits: Vec<SearchHit>,
    semantic_hits: Vec<SearchHit>,
    limit: usize,
) -> Vec<SearchHit> {
    if limit == 0 {
        return Vec::new();
    }

    let mut merged: FxHashMap<NodeId, SearchHit> = FxHashMap::default();

    add_ranked_hits(&mut merged, lexical_hits, "bm25");
    add_ranked_hits(&mut merged, semantic_hits, "semantic");

    let mut hits: Vec<SearchHit> = merged.into_values().collect();
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.node_id.as_str().cmp(b.node_id.as_str()))
    });
    hits.truncate(limit);
    for (idx, hit) in hits.iter_mut().enumerate() {
        hit.rank = idx + 1;
    }
    hits
}

fn add_ranked_hits(
    merged: &mut FxHashMap<NodeId, SearchHit>,
    hits: Vec<SearchHit>,
    source_name: &str,
) {
    for (idx, hit) in hits.into_iter().enumerate() {
        let contribution = 1.0 / (RRF_K + idx as f32 + 1.0);
        let source_score = match source_name {
            "bm25" => hit.bm25_score.unwrap_or(hit.score),
            "semantic" => hit.semantic_score.unwrap_or(hit.score),
            _ => hit.score,
        };

        let entry = merged.entry(hit.node_id.clone()).or_insert_with(|| {
            let mut seeded = hit.clone();
            seeded.score = 0.0;
            seeded.rank = 0;
            seeded.sources.clear();
            seeded.bm25_score = None;
            seeded.semantic_score = None;
            seeded
        });

        entry.score += contribution;
        if !entry.sources.iter().any(|source| source == source_name) {
            entry.sources.push(source_name.to_string());
        }
        match source_name {
            "bm25" => entry.bm25_score = Some(source_score),
            "semantic" => entry.semantic_score = Some(source_score),
            _ => {}
        }
    }
}
