use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::convert::Infallible;
use std::mem::size_of;

use cih_core::{Node, NodeId, NodeKind, Range};

use crate::is_searchable_kind;
use crate::rrf::SearchHit;
use crate::tokenize::{tokenize, Tokenizer};

pub(crate) const K1: f32 = 1.2;
pub(crate) const B: f32 = 0.75;

#[derive(Clone, Debug)]
pub struct IndexedDoc {
    pub node_id: NodeId,
    pub kind: NodeKind,
    pub name: String,
    pub qualified_name: Option<String>,
    pub file_id: u32,
    pub range: Range,
}

#[derive(Clone, Debug, Default)]
pub struct SearchIndex {
    pub(crate) docs: Vec<IndexedDoc>,
    pub(crate) files: Vec<String>,
    pub(crate) avg_doc_len: f32,
    pub(crate) postings: HashMap<String, Box<[(u32, u32)]>>,
    pub(crate) doc_len: Vec<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct SearchIndexSizeBreakdown {
    pub docs_struct_bytes: usize,
    pub node_id_bytes: usize,
    pub name_bytes: usize,
    pub qualified_name_bytes: usize,
    pub file_bytes: usize,
    pub distinct_files: usize,
    /// Kept explicit in reports to prove synthesized document text is not retained.
    pub text_bytes: usize,
    /// Kept explicit in reports to prove document frequency is derived from postings.
    pub doc_freq_bytes: usize,
    pub postings_table_bytes: usize,
    pub postings_key_bytes: usize,
    pub postings_payload_bytes: usize,
    pub postings_bytes: usize,
    pub doc_len_bytes: usize,
    pub total_bytes: usize,
}

impl SearchIndex {
    /// Compatibility entry point for callers that retain a node slice.
    pub fn build(nodes: &[Node]) -> Self {
        Self::build_owned(nodes.iter().cloned())
    }

    /// Build while consuming nodes one at a time. Retained strings can move into
    /// the index instead of requiring a second full node collection.
    pub fn build_owned<I>(nodes: I) -> Self
    where
        I: IntoIterator<Item = Node>,
    {
        match Self::try_build(nodes.into_iter().map(Ok::<Node, Infallible>)) {
            Ok(index) => index,
            Err(error) => match error {},
        }
    }

    /// Build from a fallible streaming source such as `nodes.jsonl`.
    pub fn try_build<I, E>(nodes: I) -> Result<Self, E>
    where
        I: IntoIterator<Item = Result<Node, E>>,
    {
        let mut builder = SearchIndexBuilder::default();
        for node in nodes {
            builder.push(node?);
        }
        Ok(builder.finish())
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    pub fn size_breakdown(&self) -> SearchIndexSizeBreakdown {
        let postings_table_bytes = self
            .postings
            .capacity()
            .saturating_mul(size_of::<(String, Box<[(u32, u32)]>)>());
        let postings_key_bytes = self
            .postings
            .keys()
            .fold(0usize, |total, key| total.saturating_add(key.capacity()));
        let postings_payload_bytes = self.postings.values().fold(0usize, |total, postings| {
            total.saturating_add(postings.len().saturating_mul(size_of::<(u32, u32)>()))
        });
        let mut breakdown = SearchIndexSizeBreakdown {
            docs_struct_bytes: self.docs.capacity().saturating_mul(size_of::<IndexedDoc>()),
            file_bytes: self.files.capacity().saturating_mul(size_of::<String>()),
            distinct_files: self.files.len(),
            postings_table_bytes,
            postings_key_bytes,
            postings_payload_bytes,
            postings_bytes: postings_table_bytes
                .saturating_add(postings_key_bytes)
                .saturating_add(postings_payload_bytes),
            doc_len_bytes: self.doc_len.capacity().saturating_mul(size_of::<u32>()),
            ..SearchIndexSizeBreakdown::default()
        };
        for doc in &self.docs {
            breakdown.node_id_bytes = breakdown
                .node_id_bytes
                .saturating_add(doc.node_id.as_str().len());
            breakdown.name_bytes = breakdown.name_bytes.saturating_add(doc.name.capacity());
            breakdown.qualified_name_bytes = breakdown
                .qualified_name_bytes
                .saturating_add(doc.qualified_name.as_ref().map_or(0, String::capacity));
        }
        breakdown.file_bytes = self.files.iter().fold(breakdown.file_bytes, |total, file| {
            total.saturating_add(file.capacity())
        });
        breakdown.total_bytes = size_of::<Self>()
            .saturating_add(breakdown.docs_struct_bytes)
            .saturating_add(breakdown.node_id_bytes)
            .saturating_add(breakdown.name_bytes)
            .saturating_add(breakdown.qualified_name_bytes)
            .saturating_add(breakdown.file_bytes)
            .saturating_add(breakdown.postings_bytes)
            .saturating_add(breakdown.doc_len_bytes);
        breakdown
    }

    /// Conservative retained-memory estimate for process cache accounting.
    pub fn estimated_size_bytes(&self) -> usize {
        self.size_breakdown().total_bytes
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
        let mut scores = vec![0.0_f32; self.docs.len()];
        for token in &query_tokens {
            let Some(postings) = self.postings.get(token.as_str()) else {
                continue;
            };
            let token_idf = idf(n, postings.len() as f32);
            for &(doc_idx, tf) in postings.iter() {
                let doc_idx = doc_idx as usize;
                let tf = tf as f32;
                let dl = self.doc_len[doc_idx] as f32;
                let contribution =
                    token_idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * (dl / avg)));
                scores[doc_idx] += contribution;
            }
        }

        let mut top = BinaryHeap::with_capacity(limit.saturating_add(1));
        for (doc_idx, &score) in scores.iter().enumerate() {
            if score <= 0.0 {
                continue;
            }
            let candidate = SearchCandidate {
                doc_idx,
                score,
                node_id: self.docs[doc_idx].node_id.as_str(),
            };
            if top.len() < limit {
                top.push(candidate);
            } else if top.peek().is_some_and(|worst| candidate < *worst) {
                top.pop();
                top.push(candidate);
            }
        }

        let mut candidates = top.into_vec();
        candidates.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.node_id.cmp(right.node_id))
        });
        candidates
            .into_iter()
            .enumerate()
            .map(|(rank, candidate)| {
                let doc = &self.docs[candidate.doc_idx];
                let file = self
                    .files
                    .get(doc.file_id as usize)
                    .cloned()
                    .unwrap_or_default();
                let mut hit = SearchHit::from_parts(
                    doc.node_id.clone(),
                    doc.kind,
                    doc.name.clone(),
                    doc.qualified_name.clone(),
                    file,
                    doc.range,
                    candidate.score,
                    "bm25",
                );
                hit.rank = rank + 1;
                hit
            })
            .collect()
    }
}

#[derive(Default)]
struct SearchIndexBuilder {
    docs: Vec<IndexedDoc>,
    files: Vec<String>,
    file_ids: HashMap<String, u32>,
    postings: HashMap<String, Vec<(u32, u32)>>,
    doc_len: Vec<u32>,
    total_len: u64,
    tokenizer: Tokenizer,
    tokens: Vec<String>,
    term_freq: HashMap<String, u32>,
}

impl SearchIndexBuilder {
    fn push(&mut self, node: Node) {
        if !is_searchable_kind(node.kind) {
            return;
        }

        self.tokens.clear();
        collect_node_tokens(&node, &mut self.tokenizer, &mut self.tokens);
        if self.tokens.is_empty() {
            return;
        }

        let doc_idx = checked_doc_ordinal(self.docs.len());
        let token_count = u32::try_from(self.tokens.len())
            .expect("search document supports at most u32::MAX tokens");
        self.total_len = self.total_len.saturating_add(u64::from(token_count));

        self.term_freq.clear();
        for token in self.tokens.drain(..) {
            *self.term_freq.entry(token).or_insert(0) += 1;
        }
        for (term, frequency) in self.term_freq.drain() {
            self.postings
                .entry(term)
                .or_default()
                .push((doc_idx, frequency));
        }

        let file_id = if let Some(&file_id) = self.file_ids.get(node.file.as_str()) {
            file_id
        } else {
            let file_id = u32::try_from(self.files.len())
                .expect("search index supports at most u32::MAX distinct files");
            self.file_ids.insert(node.file.clone(), file_id);
            self.files.push(node.file.clone());
            file_id
        };
        self.doc_len.push(token_count);
        self.docs.push(IndexedDoc {
            node_id: node.id,
            kind: node.kind,
            name: node.name,
            qualified_name: node.qualified_name,
            file_id,
            range: node.range,
        });
    }

    fn finish(mut self) -> SearchIndex {
        self.docs.shrink_to_fit();
        self.files.shrink_to_fit();
        self.doc_len.shrink_to_fit();
        let avg_doc_len = if self.docs.is_empty() {
            0.0
        } else {
            self.total_len as f32 / self.docs.len() as f32
        };
        let postings = self
            .postings
            .drain()
            .map(|(term, mut values)| {
                values.shrink_to_fit();
                (term, values.into_boxed_slice())
            })
            .collect();
        SearchIndex {
            docs: self.docs,
            files: self.files,
            avg_doc_len,
            postings,
            doc_len: self.doc_len,
        }
    }
}

fn checked_doc_ordinal(documents: usize) -> u32 {
    u32::try_from(documents).expect("search index supports at most u32::MAX documents")
}

#[derive(Clone, Copy, Debug)]
struct SearchCandidate<'a> {
    doc_idx: usize,
    score: f32,
    node_id: &'a str,
}

impl PartialEq for SearchCandidate<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.doc_idx == other.doc_idx
            && self.score.to_bits() == other.score.to_bits()
            && self.node_id == other.node_id
    }
}

impl Eq for SearchCandidate<'_> {}

impl PartialOrd for SearchCandidate<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SearchCandidate<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| self.node_id.cmp(other.node_id))
            .then_with(|| self.doc_idx.cmp(&other.doc_idx))
    }
}

/// BM25 index over arbitrary text documents, addressed by build-order ordinal.
#[derive(Clone, Debug, Default)]
pub struct TextIndex {
    num_docs: usize,
    indexed: usize,
    avg_doc_len: f32,
    postings: HashMap<String, Box<[(u32, u32)]>>,
    doc_len: Vec<u32>,
}

impl TextIndex {
    pub fn build<'a, I>(docs: I) -> Self
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut postings: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
        let mut doc_len = Vec::new();
        let mut total_len = 0u64;
        let mut indexed = 0usize;
        let mut num_docs = 0usize;
        let mut tokenizer = Tokenizer::default();
        let mut tokens = Vec::new();
        let mut term_freq = HashMap::new();

        for text in docs {
            let ordinal =
                u32::try_from(num_docs).expect("text index supports at most u32::MAX documents");
            num_docs += 1;
            tokens.clear();
            tokenizer.tokenize_into(text, &mut tokens);
            let token_count = u32::try_from(tokens.len())
                .expect("text document supports at most u32::MAX tokens");
            total_len = total_len.saturating_add(u64::from(token_count));
            doc_len.push(token_count);
            if tokens.is_empty() {
                continue;
            }
            indexed += 1;

            term_freq.clear();
            for token in tokens.drain(..) {
                *term_freq.entry(token).or_insert(0) += 1;
            }
            for (term, frequency) in term_freq.drain() {
                postings.entry(term).or_default().push((ordinal, frequency));
            }
        }

        doc_len.shrink_to_fit();
        let avg_doc_len = if indexed == 0 {
            0.0
        } else {
            total_len as f32 / indexed as f32
        };
        let postings = postings
            .into_iter()
            .map(|(term, mut values)| {
                values.shrink_to_fit();
                (term, values.into_boxed_slice())
            })
            .collect();
        Self {
            num_docs,
            indexed,
            avg_doc_len,
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

    pub fn estimated_size_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(postings_weight(&self.postings))
            .saturating_add(self.doc_len.capacity().saturating_mul(size_of::<u32>()))
    }

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
        for token in &query_tokens {
            let Some(postings) = self.postings.get(token.as_str()) else {
                continue;
            };
            let token_idf = idf(n, postings.len() as f32);
            for &(ordinal, tf) in postings.iter() {
                let ordinal = ordinal as usize;
                let tf = tf as f32;
                let dl = self.doc_len[ordinal] as f32;
                scores[ordinal] +=
                    token_idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * (dl / avg)));
            }
        }

        let mut top = BinaryHeap::with_capacity(limit.saturating_add(1));
        for (ordinal, &score) in scores.iter().enumerate() {
            if score <= 0.0 {
                continue;
            }
            let candidate = TextCandidate { ordinal, score };
            if top.len() < limit {
                top.push(candidate);
            } else if top.peek().is_some_and(|worst| candidate < *worst) {
                top.pop();
                top.push(candidate);
            }
        }
        let mut hits = top.into_vec();
        hits.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.ordinal.cmp(&right.ordinal))
        });
        hits.into_iter()
            .map(|candidate| (candidate.ordinal, candidate.score))
            .collect()
    }
}

#[derive(Clone, Copy, Debug)]
struct TextCandidate {
    ordinal: usize,
    score: f32,
}

impl PartialEq for TextCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.ordinal == other.ordinal && self.score.to_bits() == other.score.to_bits()
    }
}

impl Eq for TextCandidate {}

impl PartialOrd for TextCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TextCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| self.ordinal.cmp(&other.ordinal))
    }
}

fn postings_weight(values: &HashMap<String, Box<[(u32, u32)]>>) -> usize {
    values.iter().fold(
        values
            .capacity()
            .saturating_mul(size_of::<(String, Box<[(u32, u32)]>)>()),
        |total, (key, postings)| {
            total
                .saturating_add(key.capacity())
                .saturating_add(postings.len().saturating_mul(size_of::<(u32, u32)>()))
        },
    )
}

fn idf(total_docs: f32, matching_docs: f32) -> f32 {
    ((total_docs - matching_docs + 0.5) / (matching_docs + 0.5) + 1.0).ln()
}

fn collect_node_tokens(node: &Node, tokenizer: &mut Tokenizer, output: &mut Vec<String>) {
    tokenizer.tokenize_into(node.kind.label(), output);
    tokenizer.tokenize_into(&node.name, output);
    if let Some(qualified_name) = &node.qualified_name {
        tokenizer.tokenize_into(qualified_name, output);
    }
    tokenizer.tokenize_into(node.id.as_str(), output);
    tokenizer.tokenize_into(&node.file, output);

    let Some(props) = &node.props else {
        return;
    };
    if matches!(node.kind, NodeKind::Route) {
        if let Some(method) = props.get("httpMethod").and_then(|value| value.as_str()) {
            tokenizer.tokenize_into(method, output);
        }
        if let Some(path) = props.get("path").and_then(|value| value.as_str()) {
            tokenizer.tokenize_into(path, output);
            for segment in path
                .split('/')
                .filter(|segment| !segment.is_empty() && !segment.starts_with('{'))
            {
                tokenizer.tokenize_into(segment, output);
            }
        }
        if let Some(handler) = props.get("handler").and_then(|value| value.as_str()) {
            tokenizer.tokenize_into(handler, output);
        }
    }
    if matches!(node.kind, NodeKind::IntegrationRoute) {
        if let Some(uri) = props.get("uri").and_then(|value| value.as_str()) {
            tokenizer.tokenize_into(uri, output);
        }
        if let Some(source) = props.get("source").and_then(|value| value.as_str()) {
            tokenizer.tokenize_into(source, output);
        }
    }
    if matches!(node.kind, NodeKind::MessageDestination) {
        if let Some(destination_type) = props
            .get("destination_type")
            .and_then(|value| value.as_str())
        {
            tokenizer.tokenize_into(destination_type, output);
        }
    }
}

#[cfg(test)]
mod ordinal_tests {
    use super::checked_doc_ordinal;

    #[test]
    #[cfg(target_pointer_width = "64")]
    #[should_panic(expected = "search index supports at most u32::MAX documents")]
    fn document_ordinal_overflow_is_rejected() {
        checked_doc_ordinal(u32::MAX as usize + 1);
    }
}
