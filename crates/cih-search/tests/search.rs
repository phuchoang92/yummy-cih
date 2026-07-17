use cih_core::{Node, NodeId, NodeKind, Range};
use cih_search::{rrf_merge, tokenize, SearchHit, SearchIndex};

fn node(id: &str, kind: NodeKind, name: &str, qualified_name: Option<&str>) -> Node {
    Node {
        id: NodeId::new(id),
        kind,
        name: name.into(),
        qualified_name: qualified_name.map(str::to_string),
        file: "src/main/java/com/acme/OwnerService.java".into(),
        range: Range {
            start_line: 7,
            start_col: 0,
            end_line: 9,
            end_col: 1,
        },
        props: None,
    }
}

#[test]
fn bm25_exact_match_ranks_first() {
    let nodes = vec![
        node(
            "Method:com.acme.OwnerService#findAll/0",
            NodeKind::Method,
            "findAll",
            Some("com.acme.OwnerService.findAll"),
        ),
        node(
            "Method:com.acme.OwnerService#save/1",
            NodeKind::Method,
            "save",
            Some("com.acme.OwnerService.save"),
        ),
    ];

    let index = SearchIndex::build(&nodes);
    let hits = index.search("owner service find all", 10);

    assert_eq!(
        hits[0].node_id.as_str(),
        "Method:com.acme.OwnerService#findAll/0"
    );
    assert!(hits[0].bm25_score.unwrap() > hits[1].bm25_score.unwrap());
}

#[test]
fn tokenizer_splits_camel_and_punctuation() {
    let tokens = tokenize("OwnerService#findAll/2");

    assert_eq!(tokens, vec!["owner", "service", "find", "all"]);
}

#[test]
fn empty_corpus_search_returns_empty_hits() {
    let index = SearchIndex::build(&[]);

    assert!(index.search("anything", 10).is_empty());
}

#[test]
fn rrf_single_first_rank_scores_one_over_sixty_one() {
    let hit = SearchHit::from_parts(
        NodeId::new("Class:com.acme.OwnerService"),
        NodeKind::Class,
        "OwnerService".into(),
        Some("com.acme.OwnerService".into()),
        "src/main/java/com/acme/OwnerService.java".into(),
        Range::default(),
        42.0,
        "bm25",
    );

    let fused = rrf_merge(vec![hit], Vec::new(), 10);

    assert!((fused[0].score - (1.0 / 61.0)).abs() < f32::EPSILON);
    assert_eq!(fused[0].rank, 1);
}

#[test]
fn rrf_combined_item_wins_over_single_source_items() {
    let lexical = vec![
        SearchHit::from_parts(
            NodeId::new("Class:A"),
            NodeKind::Class,
            "A".into(),
            None,
            "A.java".into(),
            Range::default(),
            12.0,
            "bm25",
        ),
        SearchHit::from_parts(
            NodeId::new("Class:B"),
            NodeKind::Class,
            "B".into(),
            None,
            "B.java".into(),
            Range::default(),
            11.0,
            "bm25",
        ),
    ];
    let semantic = vec![
        SearchHit::from_parts(
            NodeId::new("Class:B"),
            NodeKind::Class,
            "B".into(),
            None,
            "B.java".into(),
            Range::default(),
            0.9,
            "semantic",
        ),
        SearchHit::from_parts(
            NodeId::new("Class:C"),
            NodeKind::Class,
            "C".into(),
            None,
            "C.java".into(),
            Range::default(),
            0.8,
            "semantic",
        ),
    ];

    let fused = rrf_merge(lexical, semantic, 10);

    assert_eq!(fused[0].node_id.as_str(), "Class:B");
    assert_eq!(fused[0].sources, vec!["bm25", "semantic"]);
}

#[test]
fn text_index_scores_generic_documents() {
    use cih_search::TextIndex;

    let docs = [
        "Loan repayment schedules and interest accrual",
        "Invoice generation for monthly billing",
        "",
    ];
    let index = TextIndex::build(docs.iter().copied());
    assert_eq!(index.len(), 3);

    let hits = index.search("repayment schedule", 10);
    assert_eq!(hits[0].0, 0);
    // The empty document never matches.
    assert!(hits.iter().all(|(ordinal, _)| *ordinal != 2));

    assert!(index.search("", 10).is_empty());
    assert!(index.search("repayment", 0).is_empty());
    assert!(TextIndex::build(std::iter::empty())
        .search("x", 5)
        .is_empty());
}

/// Naive full-scan BM25 mirroring the pre-inverted-index algorithm exactly, used
/// as the correctness reference for the inverted `TextIndex`. `SearchIndex` shares
/// the identical scoring math, so parity here guards both.
fn naive_text_search(docs: &[&str], query: &str, limit: usize) -> Vec<(usize, f32)> {
    use std::collections::{HashMap, HashSet};
    const K1: f32 = 1.2;
    const B: f32 = 0.75;
    let idf = |total: f32, matching: f32| ((total - matching + 0.5) / (matching + 0.5) + 1.0).ln();

    let token_docs: Vec<Vec<String>> = docs.iter().map(|d| tokenize(d)).collect();
    if token_docs.is_empty() || limit == 0 {
        return Vec::new();
    }
    let query_tokens = tokenize(query);
    if query_tokens.is_empty() {
        return Vec::new();
    }
    let indexed = token_docs.iter().filter(|t| !t.is_empty()).count();
    let total_len: usize = token_docs.iter().map(|t| t.len()).sum();
    let avg_doc_len = if indexed == 0 {
        0.0
    } else {
        total_len as f32 / indexed as f32
    };
    let mut doc_freq: HashMap<String, usize> = HashMap::new();
    for tokens in &token_docs {
        let mut seen = HashSet::new();
        for token in tokens {
            if seen.insert(token.as_str()) {
                *doc_freq.entry(token.clone()).or_insert(0) += 1;
            }
        }
    }

    let mut hits = Vec::new();
    for (ordinal, tokens) in token_docs.iter().enumerate() {
        if tokens.is_empty() {
            continue;
        }
        let mut term_freq: HashMap<&str, usize> = HashMap::new();
        for token in tokens {
            *term_freq.entry(token.as_str()).or_insert(0) += 1;
        }
        let mut score = 0.0;
        for token in &query_tokens {
            let tf = *term_freq.get(token.as_str()).unwrap_or(&0) as f32;
            if tf == 0.0 {
                continue;
            }
            let df = *doc_freq.get(token).unwrap_or(&0) as f32;
            score += idf(indexed as f32, df) * (tf * (K1 + 1.0))
                / (tf + K1 * (1.0 - B + B * (tokens.len() as f32 / avg_doc_len.max(1.0))));
        }
        if score > 0.0 {
            hits.push((ordinal, score));
        }
    }
    hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    hits.truncate(limit);
    hits
}

#[test]
fn inverted_index_matches_naive_reference() {
    use cih_search::TextIndex;

    // Deterministic pseudo-random corpus over a small vocabulary so terms repeat
    // across docs, doc lengths vary, and some docs are empty — the conditions a
    // hand-written fixture would miss. Byte-identical scores are required.
    let vocab = [
        "loan",
        "repayment",
        "schedule",
        "interest",
        "accrual",
        "invoice",
        "billing",
        "customer",
        "account",
        "balance",
        "transfer",
        "audit",
        "report",
        "ledger",
    ];
    let mut state: u64 = 0x9E3779B97F4A7C15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let corpus: Vec<String> = (0..400)
        .map(|_| {
            let len = (next() % 12) as usize; // 0..11 → includes empty docs
            (0..len)
                .map(|_| vocab[(next() as usize) % vocab.len()])
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    let doc_refs: Vec<&str> = corpus.iter().map(String::as_str).collect();

    let index = TextIndex::build(doc_refs.iter().copied());
    assert_eq!(index.len(), corpus.len());

    // Queries include single terms, multi-term, and a repeated term (which must
    // double-count, matching the reference) and a term absent from the vocab.
    let queries = [
        "loan",
        "repayment schedule",
        "interest accrual balance transfer",
        "audit audit report",
        "nonexistentterm",
        "loan repayment loan",
    ];
    for q in queries {
        for &limit in &[1usize, 5, 25, 1000] {
            let got = index.search(q, limit);
            let want = naive_text_search(&doc_refs, q, limit);
            assert_eq!(got, want, "mismatch for query {q:?} limit {limit}");
        }
    }
}
