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
    assert!(TextIndex::build(std::iter::empty()).search("x", 5).is_empty());
}
