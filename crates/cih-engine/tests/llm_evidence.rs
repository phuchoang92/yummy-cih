use cih_core::{Edge, EdgeKind, NodeId, NodeKind, Range};
use cih_engine_lib::llm::evidence::*;
use cih_engine_lib::llm::split_text_chunks;
use cih_wiki::WikiGraph;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_ID: AtomicU64 = AtomicU64::new(0);

fn node(id: &str, kind: NodeKind, name: &str, file: &str, line: u32) -> cih_core::Node {
    cih_core::Node {
        id: NodeId::new(id.to_string()),
        kind,
        name: name.to_string(),
        qualified_name: None,
        file: file.to_string(),
        range: Range {
            start_line: line,
            end_line: line,
            ..Range::default()
        },
        props: None,
    }
}

fn temp_repo() -> PathBuf {
    let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("cih-evidence-test-{}-{id}", std::process::id()));
    std::fs::create_dir_all(root.join("src")).unwrap();
    root
}

#[test]
fn split_md_and_txt_chunks_at_paragraphs() {
    let chunks = split_text_chunks("one\n\n two\n\nthree", 400);
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].contains("one"));
    assert!(chunks[0].contains("three"));
}

#[test]
fn safe_repo_path_rejects_escaping_paths() {
    let root = temp_repo();
    std::fs::write(root.join("src/Foo.java"), "class Foo {}").unwrap();
    assert!(safe_repo_path(&root, "src/Foo.java").is_some());
    assert!(safe_repo_path(&root, "../secret.java").is_none());
    assert!(safe_repo_path(&root, "/tmp/secret.java").is_none());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn evidence_pack_includes_routes_and_tables() {
    let method = node(
        "Method:com.example.OrderService#find/0",
        NodeKind::Method,
        "find",
        "src/OrderService.java",
        1,
    );
    let community = node("Community:0", NodeKind::Community, "order-service", "", 0);
    let route = cih_core::Node {
        props: Some(serde_json::json!({"httpMethod": "GET", "path": "/orders"})),
        ..node("Route:GET:/orders", NodeKind::Route, "GET /orders", "", 0)
    };
    let query = node("DbQuery:q", NodeKind::DbQuery, "q", "", 0);
    let table = node("DbTable:ORDERS", NodeKind::DbTable, "ORDERS", "", 0);
    let edges = [
        Edge {
            src: method.id.clone(),
            dst: route.id.clone(),
            kind: EdgeKind::HandlesRoute,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        },
        Edge {
            src: method.id.clone(),
            dst: query.id.clone(),
            kind: EdgeKind::ExecutesQuery,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        },
        Edge {
            src: query.id.clone(),
            dst: table.id.clone(),
            kind: EdgeKind::ReadsTable,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        },
    ];
    let comm_edges = [Edge {
        src: method.id.clone(),
        dst: community.id.clone(),
        kind: EdgeKind::MemberOf,
        confidence: 1.0,
        reason: String::new(),
        props: None,
    }];
    let graph = WikiGraph::build(
        &[method, route, query, table],
        &edges,
        std::slice::from_ref(&community),
        &comm_edges,
    );
    let pack = build_evidence_pack(None, &graph, &community, &EvidenceCorpus::default());
    let rendered = pack.render();
    assert!(rendered.contains("[R1] GET /orders"));
    assert!(rendered.contains("[T1] ORDERS (read)"));
}

#[test]
fn evidence_pack_includes_only_business_processes() {
    let community = node("Community:0", NodeKind::Community, "order-service", "", 0);
    let business = cih_core::Node {
        props: Some(serde_json::json!({
            "label": "Create order",
            "communities": ["Community:0"],
            "business_flow": true,
            "entrypoint_kind": "http_route",
            "step_count": 3,
            "route_method": "POST",
            "route_path": "/orders"
        })),
        ..node(
            "Process:create-order",
            NodeKind::Process,
            "Create order",
            "",
            0,
        )
    };
    let internal = cih_core::Node {
        props: Some(serde_json::json!({
            "label": "Internal fanout",
            "communities": ["Community:0"],
            "business_flow": false,
            "entrypoint_kind": "fanout",
            "step_count": 4
        })),
        ..node(
            "Process:internal-fanout",
            NodeKind::Process,
            "Internal fanout",
            "",
            0,
        )
    };
    let graph = WikiGraph::build(&[], &[], &[community.clone(), business, internal], &[]);
    let pack = build_evidence_pack(None, &graph, &community, &EvidenceCorpus::default());
    let rendered = pack.render();
    assert!(rendered.contains("[P1] Create order"));
    assert!(rendered.contains("route POST /orders"));
    assert!(!rendered.contains("Internal fanout"));
}

#[test]
fn brd_matching_requires_two_distinct_terms_and_caps_to_two_chunks() {
    let method = node(
        "Method:com.example.OrderService#cancel/0",
        NodeKind::Method,
        "cancel",
        "modules/order/OrderService.java",
        1,
    );
    let community = node("Community:0", NodeKind::Community, "order-service", "", 0);
    let comm_edges = [Edge {
        src: method.id.clone(),
        dst: community.id.clone(),
        kind: EdgeKind::MemberOf,
        confidence: 1.0,
        reason: String::new(),
        props: None,
    }];
    let graph = WikiGraph::build(&[method], &[], std::slice::from_ref(&community), &comm_edges);
    let corpus = EvidenceCorpus {
        file_count: 1,
        chunks: vec![
            EvidenceChunk {
                source: "brd.md#1".into(),
                text: "order service workflow".into(),
            },
            EvidenceChunk {
                source: "brd.md#2".into(),
                text: "order service approval".into(),
            },
            EvidenceChunk {
                source: "brd.md#3".into(),
                text: "only order".into(),
            },
        ],
    };
    let pack = build_evidence_pack(None, &graph, &community, &corpus);
    let brd_count = pack
        .items
        .iter()
        .filter(|i| i.kind == EvidenceKind::Brd)
        .count();
    assert_eq!(brd_count, 2);
    assert!(pack.render().contains("[B1]"));
    assert!(!pack.render().contains("only order"));
}

#[test]
fn source_snippet_selection_is_deterministic_and_capped() {
    let root = temp_repo();
    std::fs::write(
        root.join("src/A.java"),
        "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n",
    )
    .unwrap();
    std::fs::write(root.join("src/B.java"), "a\nb\nc\nd\ne\n").unwrap();
    let m1 = node("Method:a.A#m1/0", NodeKind::Method, "m1", "src/A.java", 2);
    let m2 = node("Method:a.A#m2/0", NodeKind::Method, "m2", "src/A.java", 4);
    let m3 = node("Method:a.B#m1/0", NodeKind::Method, "m1", "src/B.java", 1);
    let community = node("Community:0", NodeKind::Community, "shared", "", 0);
    let comm_edges = [
        Edge {
            src: m1.id.clone(),
            dst: community.id.clone(),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        },
        Edge {
            src: m2.id.clone(),
            dst: community.id.clone(),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        },
        Edge {
            src: m3.id.clone(),
            dst: community.id.clone(),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        },
    ];
    let graph = WikiGraph::build(&[m1, m2, m3], &[], std::slice::from_ref(&community), &comm_edges);
    let pack = build_evidence_pack(Some(&root), &graph, &community, &EvidenceCorpus::default());
    let rendered = pack.render();
    assert!(rendered.contains("[S1] src/A.java:2-11"));
    assert!(rendered.len() <= MAX_EVIDENCE_CHARS);
    let _ = std::fs::remove_dir_all(root);
}
