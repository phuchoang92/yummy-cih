use std::collections::{HashMap, HashSet};

use cih_core::{Node, NodeId, NodeKind, Range};
use serde::{Deserialize, Serialize};

use crate::is_searchable_kind;
use crate::rrf::SearchHit;
use crate::tokenize::tokenize;

const K1: f32 = 1.2;
const B: f32 = 0.75;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexedDoc {
    pub node_id: NodeId,
    pub kind: NodeKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
    pub file: String,
    pub range: Range,
    pub text: String,
    #[serde(skip)]
    tokens: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchIndex {
    docs: Vec<IndexedDoc>,
    avg_doc_len: f32,
    doc_freq: HashMap<String, usize>,
}

impl SearchIndex {
    pub fn build(nodes: &[Node]) -> Self {
        let mut docs = Vec::new();
        let mut doc_freq: HashMap<String, usize> = HashMap::new();
        let mut total_len = 0usize;

        for node in nodes.iter().filter(|node| is_searchable_kind(node.kind)) {
            let text = node_text(node);
            let tokens = tokenize(&text);
            if tokens.is_empty() {
                continue;
            }
            total_len += tokens.len();

            let mut seen = HashSet::new();
            for token in &tokens {
                if seen.insert(token.as_str()) {
                    *doc_freq.entry(token.clone()).or_insert(0) += 1;
                }
            }

            docs.push(IndexedDoc {
                node_id: node.id.clone(),
                kind: node.kind,
                name: node.name.clone(),
                qualified_name: node.qualified_name.clone(),
                file: node.file.clone(),
                range: node.range,
                text,
                tokens,
            });
        }

        let avg_doc_len = if docs.is_empty() {
            0.0
        } else {
            total_len as f32 / docs.len() as f32
        };

        Self {
            docs,
            avg_doc_len,
            doc_freq,
        }
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        if self.docs.is_empty() || limit == 0 {
            return Vec::new();
        }
        let query_tokens = tokenize(query);
        if query_tokens.is_empty() {
            return Vec::new();
        }

        let mut hits = Vec::new();
        for doc in &self.docs {
            let mut term_freq: HashMap<&str, usize> = HashMap::new();
            for token in &doc.tokens {
                *term_freq.entry(token.as_str()).or_insert(0) += 1;
            }

            let mut score = 0.0;
            for token in &query_tokens {
                let tf = *term_freq.get(token.as_str()).unwrap_or(&0) as f32;
                if tf == 0.0 {
                    continue;
                }
                let df = *self.doc_freq.get(token).unwrap_or(&0) as f32;
                score += idf(self.docs.len() as f32, df) * (tf * (K1 + 1.0))
                    / (tf
                        + K1 * (1.0 - B
                            + B * (doc.tokens.len() as f32 / self.avg_doc_len.max(1.0))));
            }

            if score > 0.0 {
                hits.push(SearchHit::from_parts(
                    doc.node_id.clone(),
                    doc.kind,
                    doc.name.clone(),
                    doc.qualified_name.clone(),
                    doc.file.clone(),
                    doc.range,
                    score,
                    "bm25",
                ));
            }
        }

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
}

fn idf(total_docs: f32, matching_docs: f32) -> f32 {
    ((total_docs - matching_docs + 0.5) / (matching_docs + 0.5) + 1.0).ln()
}

fn node_text(node: &Node) -> String {
    let mut parts = Vec::new();
    parts.push(node.kind.label().to_string());
    parts.push(node.name.clone());
    if let Some(qualified_name) = &node.qualified_name {
        parts.push(qualified_name.clone());
    }
    parts.push(node.id.as_str().to_string());
    parts.push(node.file.clone());
    // Enrich with props for higher-signal node kinds.
    if let Some(props) = &node.props {
        // Route: include HTTP method and path segments so "GET /orders" matches.
        if matches!(node.kind, NodeKind::Route) {
            if let Some(m) = props.get("httpMethod").and_then(|v| v.as_str()) {
                parts.push(m.to_string());
            }
            if let Some(p) = props.get("path").and_then(|v| v.as_str()) {
                // add both the raw path and its slash-split segments
                parts.push(p.to_string());
                for seg in p.split('/').filter(|s| !s.is_empty() && !s.starts_with('{')) {
                    parts.push(seg.to_string());
                }
            }
            if let Some(handler) = props.get("handler").and_then(|v| v.as_str()) {
                parts.push(handler.to_string());
            }
        }
        // IntegrationRoute: include uri and source for searchability.
        if matches!(node.kind, NodeKind::IntegrationRoute) {
            if let Some(uri) = props.get("uri").and_then(|v| v.as_str()) {
                parts.push(uri.to_string());
            }
            if let Some(source) = props.get("source").and_then(|v| v.as_str()) {
                parts.push(source.to_string());
            }
        }
        // MessageDestination: include destination_type and component.
        if matches!(node.kind, NodeKind::MessageDestination) {
            if let Some(dt) = props.get("destination_type").and_then(|v| v.as_str()) {
                parts.push(dt.to_string());
            }
        }
    }
    parts.join(" ")
}
