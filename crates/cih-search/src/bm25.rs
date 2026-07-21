use std::collections::HashMap;
use std::mem::size_of;

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

    /// Conservative retained-memory estimate for process cache accounting.
    pub fn estimated_size_bytes(&self) -> usize {
        let docs = self.docs.iter().fold(
            self.docs.capacity().saturating_mul(size_of::<IndexedDoc>()),
            |total, doc| {
                total
                    .saturating_add(doc.node_id.as_str().len())
                    .saturating_add(doc.name.capacity())
                    .saturating_add(doc.qualified_name.as_ref().map_or(0, String::capacity))
                    .saturating_add(doc.file.capacity())
                    .saturating_add(doc.text.capacity())
            },
        );
        size_of::<Self>()
            .saturating_add(docs)
            .saturating_add(string_usize_map_weight(&self.doc_freq))
            .saturating_add(postings_weight(&self.postings))
            .saturating_add(self.doc_len.capacity().saturating_mul(size_of::<u32>()))
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
        // A dense score buffer is substantially cheaper than a HashMap for
        // common terms that match hundreds of thousands of documents. Keep a
        // compact touched list so result construction still visits candidates
        // only, not every indexed document.
        let mut scores = vec![0.0_f32; self.docs.len()];
        let mut touched = Vec::new();
        for token in &query_tokens {
            let Some(postings) = self.postings.get(token.as_str()) else {
                continue;
            };
            let df = *self.doc_freq.get(token.as_str()).unwrap_or(&0) as f32;
            let token_idf = idf(n, df);
            for &(doc_idx, tf) in postings {
                let doc_idx = doc_idx as usize;
                let tf = tf as f32;
                let dl = self.doc_len[doc_idx] as f32;
                let contrib =
                    token_idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * (dl / avg)));
                if scores[doc_idx] == 0.0 {
                    touched.push(doc_idx);
                }
                scores[doc_idx] += contrib;
            }
        }

        let mut candidates: Vec<(usize, f32)> = touched
            .into_iter()
            .map(|doc_idx| (doc_idx, scores[doc_idx]))
            .filter(|&(_, score)| score > 0.0)
            .collect();
        let rank_order = |a: &(usize, f32), b: &(usize, f32)| {
            b.1.total_cmp(&a.1).then_with(|| {
                self.docs[a.0]
                    .node_id
                    .as_str()
                    .cmp(self.docs[b.0].node_id.as_str())
            })
        };
        // Partition in linear time and sort only the requested top-k. This
        // preserves the existing deterministic score/node-id ordering.
        if candidates.len() > limit {
            candidates.select_nth_unstable_by(limit, rank_order);
            candidates.truncate(limit);
        }
        candidates.sort_by(rank_order);

        candidates
            .into_iter()
            .enumerate()
            .map(|(rank, (doc_idx, score))| {
                let doc = &self.docs[doc_idx];
                let mut hit = SearchHit::from_parts(
                    doc.node_id.clone(),
                    doc.kind,
                    doc.name.clone(),
                    doc.qualified_name.clone(),
                    doc.file.clone(),
                    doc.range,
                    score,
                    "bm25",
                );
                hit.rank = rank + 1;
                hit
            })
            .collect()
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

    /// Conservative retained-memory estimate for process cache accounting.
    pub fn estimated_size_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(string_usize_map_weight(&self.doc_freq))
            .saturating_add(postings_weight(&self.postings))
            .saturating_add(self.doc_len.capacity().saturating_mul(size_of::<u32>()))
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
        let mut scores = vec![0.0_f32; self.num_docs];
        let mut touched = Vec::new();
        for token in &query_tokens {
            let Some(postings) = self.postings.get(token.as_str()) else {
                continue;
            };
            let df = *self.doc_freq.get(token.as_str()).unwrap_or(&0) as f32;
            let token_idf = idf(n, df);
            for &(ordinal, tf) in postings {
                let ordinal = ordinal as usize;
                let tf = tf as f32;
                let dl = self.doc_len[ordinal] as f32;
                let contrib =
                    token_idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * (dl / avg)));
                if scores[ordinal] == 0.0 {
                    touched.push(ordinal);
                }
                scores[ordinal] += contrib;
            }
        }

        let mut hits: Vec<(usize, f32)> = touched
            .into_iter()
            .map(|ordinal| (ordinal, scores[ordinal]))
            .filter(|&(_, score)| score > 0.0)
            .collect();

        let rank_order =
            |a: &(usize, f32), b: &(usize, f32)| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0));
        if hits.len() > limit {
            hits.select_nth_unstable_by(limit, rank_order);
            hits.truncate(limit);
        }
        hits.sort_by(rank_order);
        hits
    }
}

fn string_usize_map_weight(values: &HashMap<String, usize>) -> usize {
    values.iter().fold(
        values
            .capacity()
            .saturating_mul(size_of::<(String, usize)>()),
        |total, (key, _)| total.saturating_add(key.capacity()),
    )
}

fn postings_weight(values: &HashMap<String, Vec<(u32, u32)>>) -> usize {
    values.iter().fold(
        values
            .capacity()
            .saturating_mul(size_of::<(String, Vec<(u32, u32)>)>()),
        |total, (key, postings)| {
            total
                .saturating_add(key.capacity())
                .saturating_add(postings.capacity().saturating_mul(size_of::<(u32, u32)>()))
        },
    )
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
