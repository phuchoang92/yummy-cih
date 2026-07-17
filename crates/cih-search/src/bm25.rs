use std::collections::HashMap;

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
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SearchIndex {
    docs: Vec<IndexedDoc>,
    avg_doc_len: f32,
    doc_freq: HashMap<String, usize>,
    /// Inverted index: term -> [(doc_idx, term_freq)]. Only docs that contain the
    /// term appear, so `search` visits matching docs instead of scanning every doc
    /// and rebuilding a per-doc frequency map on each query.
    #[serde(default)]
    postings: HashMap<String, Vec<(u32, u32)>>,
    /// Token count per doc, parallel to `docs` (BM25 length normalization).
    #[serde(default)]
    doc_len: Vec<u32>,
}

impl SearchIndex {
    pub fn build(nodes: &[Node]) -> Self {
        let mut docs = Vec::new();
        let mut doc_freq: HashMap<String, usize> = HashMap::new();
        let mut postings: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
        let mut doc_len: Vec<u32> = Vec::new();
        let mut total_len = 0usize;

        for node in nodes.iter().filter(|node| is_searchable_kind(node.kind)) {
            let text = node_text(node);
            let tokens = tokenize(&text);
            if tokens.is_empty() {
                continue;
            }
            total_len += tokens.len();
            let doc_idx = docs.len() as u32;

            let mut term_freq: HashMap<&str, u32> = HashMap::new();
            for token in &tokens {
                *term_freq.entry(token.as_str()).or_insert(0) += 1;
            }
            for (term, &freq) in &term_freq {
                *doc_freq.entry((*term).to_string()).or_insert(0) += 1;
                postings
                    .entry((*term).to_string())
                    .or_default()
                    .push((doc_idx, freq));
            }

            doc_len.push(tokens.len() as u32);
            docs.push(IndexedDoc {
                node_id: node.id.clone(),
                kind: node.kind,
                name: node.name.clone(),
                qualified_name: node.qualified_name.clone(),
                file: node.file.clone(),
                range: node.range,
                text,
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
            postings,
            doc_len,
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

        let n = self.docs.len() as f32;
        let avg = self.avg_doc_len.max(1.0);
        // Accumulate scores only over docs that contain a query term. Query tokens
        // are iterated in order, so a doc's contributions land in the same order as
        // the previous per-doc loop — identical f32 accumulation, identical output.
        let mut scores: HashMap<u32, f32> = HashMap::new();
        for token in &query_tokens {
            let Some(postings) = self.postings.get(token.as_str()) else {
                continue;
            };
            let df = *self.doc_freq.get(token.as_str()).unwrap_or(&0) as f32;
            let token_idf = idf(n, df);
            for &(doc_idx, tf) in postings {
                let tf = tf as f32;
                let dl = self.doc_len[doc_idx as usize] as f32;
                let contrib =
                    token_idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * (dl / avg)));
                *scores.entry(doc_idx).or_insert(0.0) += contrib;
            }
        }

        let mut hits: Vec<SearchHit> = scores
            .into_iter()
            .filter(|&(_, score)| score > 0.0)
            .map(|(doc_idx, score)| {
                let doc = &self.docs[doc_idx as usize];
                SearchHit::from_parts(
                    doc.node_id.clone(),
                    doc.kind,
                    doc.name.clone(),
                    doc.qualified_name.clone(),
                    doc.file.clone(),
                    doc.range,
                    score,
                    "bm25",
                )
            })
            .collect();

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

/// BM25 index over arbitrary text documents, addressed by build-order ordinal.
///
/// Unlike [`SearchIndex`], which indexes graph nodes, this scores plain
/// strings (e.g. rendered wiki pages) and returns `(ordinal, score)` pairs.
/// Empty documents keep their ordinal so callers can map hits back to their
/// own collection, but they never match.
#[derive(Clone, Debug, Default)]
pub struct TextIndex {
    /// Total document count, including empty docs (which keep their ordinal so
    /// callers can map hits back to their own collection).
    num_docs: usize,
    /// Non-empty document count — the `N` used in IDF (matches the original,
    /// which counted non-empty docs at query time).
    indexed: usize,
    avg_doc_len: f32,
    doc_freq: HashMap<String, usize>,
    /// Inverted index: term -> [(ordinal, term_freq)].
    postings: HashMap<String, Vec<(u32, u32)>>,
    /// Token count per ordinal (0 for empty docs).
    doc_len: Vec<u32>,
}

impl TextIndex {
    pub fn build<'a, I>(docs: I) -> Self
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut doc_freq: HashMap<String, usize> = HashMap::new();
        let mut postings: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
        let mut doc_len: Vec<u32> = Vec::new();
        let mut total_len = 0usize;
        let mut indexed = 0usize;
        let mut num_docs = 0usize;

        for text in docs {
            let ordinal = num_docs as u32;
            num_docs += 1;
            let tokens = tokenize(text);
            total_len += tokens.len();
            doc_len.push(tokens.len() as u32);
            if tokens.is_empty() {
                continue;
            }
            indexed += 1;

            let mut term_freq: HashMap<&str, u32> = HashMap::new();
            for token in &tokens {
                *term_freq.entry(token.as_str()).or_insert(0) += 1;
            }
            for (term, &freq) in &term_freq {
                *doc_freq.entry((*term).to_string()).or_insert(0) += 1;
                postings
                    .entry((*term).to_string())
                    .or_default()
                    .push((ordinal, freq));
            }
        }

        let avg_doc_len = if indexed == 0 {
            0.0
        } else {
            total_len as f32 / indexed as f32
        };

        Self {
            num_docs,
            indexed,
            avg_doc_len,
            doc_freq,
            postings,
            doc_len,
        }
    }

    pub fn len(&self) -> usize {
        self.num_docs
    }

    pub fn is_empty(&self) -> bool {
        self.num_docs == 0
    }

    /// Rank documents against `query`, best first. Ties break on ordinal.
    pub fn search(&self, query: &str, limit: usize) -> Vec<(usize, f32)> {
        if self.num_docs == 0 || limit == 0 {
            return Vec::new();
        }
        let query_tokens = tokenize(query);
        if query_tokens.is_empty() {
            return Vec::new();
        }

        let n = self.indexed as f32;
        let avg = self.avg_doc_len.max(1.0);
        let mut scores: HashMap<u32, f32> = HashMap::new();
        for token in &query_tokens {
            let Some(postings) = self.postings.get(token.as_str()) else {
                continue;
            };
            let df = *self.doc_freq.get(token.as_str()).unwrap_or(&0) as f32;
            let token_idf = idf(n, df);
            for &(ordinal, tf) in postings {
                let tf = tf as f32;
                let dl = self.doc_len[ordinal as usize] as f32;
                let contrib =
                    token_idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * (dl / avg)));
                *scores.entry(ordinal).or_insert(0.0) += contrib;
            }
        }

        let mut hits: Vec<(usize, f32)> = scores
            .into_iter()
            .filter(|&(_, score)| score > 0.0)
            .map(|(ordinal, score)| (ordinal as usize, score))
            .collect();

        hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        hits.truncate(limit);
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
                for seg in p
                    .split('/')
                    .filter(|s| !s.is_empty() && !s.starts_with('{'))
                {
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
