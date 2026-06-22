use super::*;
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicUsize, Ordering as AOrdering},
    Mutex,
};

struct MockAdapter {
    responses: Mutex<VecDeque<Result<String>>>,
    call_count: AtomicUsize,
}
impl MockAdapter {
    fn new(responses: Vec<Result<String>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            call_count: AtomicUsize::new(0),
        }
    }
    fn calls(&self) -> usize {
        self.call_count.load(AOrdering::SeqCst)
    }
}
impl LlmAdapter for MockAdapter {
    fn call(&self, _key: Option<&str>, _req: &LlmRequest) -> Result<crate::llm::LlmResponse> {
        self.call_count.fetch_add(1, AOrdering::SeqCst);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(anyhow::anyhow!("no more responses")))
            .map(|text| crate::llm::LlmResponse { text })
    }
}

#[test]
fn retry_succeeds_after_two_transient_failures() {
    let valid = r#"{"po":"business value","ba":"workflow","dev":"technical"}"#.to_string();
    let adapter = MockAdapter::new(vec![
        Err(anyhow::anyhow!("transient error 1")),
        Err(anyhow::anyhow!("transient error 2")),
        Ok(valid),
    ]);
    let community = cih_core::Node {
        id: cih_core::NodeId::new("Community:test"),
        kind: cih_core::NodeKind::Community,
        name: "test".to_string(),
        qualified_name: None,
        file: String::new(),
        range: cih_core::Range::default(),
        props: None,
    };
    let graph = WikiGraph::build(&[], &[], &[community.clone()], &[]);
    let corpus = EvidenceCorpus::load(&[]).unwrap();
    let tmp = std::env::temp_dir();
    let result = enrich_one_community(
        &community,
        &graph,
        &tmp,
        &corpus,
        &adapter,
        None,
        "test-model",
        100,
        5,
        2,
        "en",
        false,
        false,
    );
    assert!(result.is_ok(), "expected Ok after retries: {:?}", result);
    assert_eq!(adapter.calls(), 3, "should have made exactly 3 calls");
}

#[test]
fn circuit_breaker_open_at_threshold() {
    assert!(
        !is_circuit_open(4, 5),
        "4 failures should not open circuit (threshold=5)"
    );
    assert!(
        is_circuit_open(5, 5),
        "5 failures should open circuit (threshold=5)"
    );
    assert!(is_circuit_open(6, 5), "6 failures should keep circuit open");
}

#[test]
fn cached_summary_returns_some_on_hash_match() {
    use std::collections::BTreeMap;
    let mut cache = BTreeMap::new();
    cache.insert(
        "comm::Payment".to_string(),
        WikiModuleCacheEntry {
            content_hash: String::new(),
            evidence_hash: "abc123".to_string(),
            page_paths: vec![],
            llm_po: Some("PO text".to_string()),
            llm_ba: Some("BA text".to_string()),
            llm_dev: Some("Dev text".to_string()),
        },
    );
    let meta = WikiMeta {
        schema_version: 1,
        repo_commit: None,
        graph_version: "v1".to_string(),
        community_version: "v1".to_string(),
        model: None,
        language: None,
        prompt_version: "1".to_string(),
        module_cache: cache,
        feature_cache: Default::default(),
    };

    let hit = cached_summary("comm::Payment", "abc123", Some(&meta));
    assert!(hit.is_some(), "matching hash should return cached summary");
    assert_eq!(hit.unwrap().po, "PO text");

    let miss_hash = cached_summary("comm::Payment", "different_hash", Some(&meta));
    assert!(miss_hash.is_none(), "different hash should return None");

    let miss_id = cached_summary("comm::Other", "abc123", Some(&meta));
    assert!(miss_id.is_none(), "unknown comm_id should return None");
}

#[test]
fn cached_feature_summary_returns_some_on_hash_match() {
    use std::collections::BTreeMap;
    let mut cache = BTreeMap::new();
    cache.insert(
        "payments".to_string(),
        FeatureMetaEntry {
            ev_hash: "ev1".to_string(),
            po_overview: "Payments overview".to_string(),
            po_capabilities: "-> Pay bills".to_string(),
            ba_process_overview: "Payment process".to_string(),
            ba_business_rules: "-> Must validate amount".to_string(),
        },
    );
    let meta = WikiMeta {
        schema_version: 1,
        repo_commit: None,
        graph_version: "v1".to_string(),
        community_version: "v1".to_string(),
        model: None,
        language: None,
        prompt_version: "1".to_string(),
        module_cache: Default::default(),
        feature_cache: cache,
    };

    let hit = cached_feature_summary("payments", "ev1", Some(&meta));
    assert_eq!(hit.unwrap().po_overview, "Payments overview");
    assert!(cached_feature_summary("payments", "other", Some(&meta)).is_none());
    assert!(cached_feature_summary("orders", "ev1", Some(&meta)).is_none());
}

#[test]
fn feature_response_parser_handles_raw_and_fenced_json() {
    let raw = r#"{
        "po_overview": "Orders overview",
        "po_capabilities": "-> Create order",
        "ba_process_overview": "Order process",
        "ba_business_rules": "-> Validate customer"
    }"#;
    let parsed = parse_feature_summary(raw).unwrap();
    assert_eq!(parsed.po_overview, "Orders overview");

    let fenced = format!("```json\n{raw}\n```");
    let parsed = parse_feature_summary(&fenced).unwrap();
    assert_eq!(parsed.ba_business_rules, "-> Validate customer");
}

#[test]
fn feature_response_parser_rejects_malformed_or_empty_output() {
    assert!(parse_feature_summary("not json").is_err());
    assert!(parse_feature_summary("{}").is_err());
}

#[test]
fn feature_prompt_json_example_is_well_formed_enough_for_models() {
    let prompt = build_feature_user_prompt("orders", "[R1] GET /orders");
    assert!(prompt.contains("\"po_capabilities\""));
    assert!(!prompt.contains("->\">"));
}

#[test]
fn feature_dry_run_does_not_call_adapter() {
    let adapter = MockAdapter::new(vec![]);
    let summary = enrich_one_feature(
        "orders",
        "[R1] GET /orders",
        &adapter,
        None,
        "model",
        100,
        5,
        0,
        false,
        true,
    )
    .unwrap();
    assert_eq!(adapter.calls(), 0);
    assert_eq!(summary.po_overview, "[dry-run] orders");
}

#[test]
fn feature_filter_keeps_only_matching_groups() {
    let mut groups = vec![
        FeatureGroup {
            feature: "payments".to_string(),
            community_ids: vec!["Community:0".to_string()],
        },
        FeatureGroup {
            feature: "orders".to_string(),
            community_ids: vec!["Community:1".to_string()],
        },
    ];
    retain_matching_feature_groups(&mut groups, &["pay".to_string()]);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].feature, "payments");
}

#[test]
fn feature_evidence_prefixes_item_ids_by_community() {
    use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};

    fn node(id: &str, kind: NodeKind, name: &str, props: Option<serde_json::Value>) -> Node {
        Node {
            id: NodeId::new(id.to_string()),
            kind,
            name: name.to_string(),
            qualified_name: None,
            file: "modules/orders/src/Controller.java".to_string(),
            range: Range::default(),
            props,
        }
    }

    let comm_a = node("Community:0", NodeKind::Community, "Orders A", None);
    let comm_b = node("Community:1", NodeKind::Community, "Orders B", None);
    let handler_a = node(
        "Method:com.example.OrderController#list/0",
        NodeKind::Method,
        "list",
        None,
    );
    let handler_b = node(
        "Method:com.example.OrderController#create/0",
        NodeKind::Method,
        "create",
        None,
    );
    let route_a = node(
        "Route:GET:/orders",
        NodeKind::Route,
        "GET /orders",
        Some(serde_json::json!({"httpMethod": "GET", "path": "/orders"})),
    );
    let route_b = node(
        "Route:POST:/orders",
        NodeKind::Route,
        "POST /orders",
        Some(serde_json::json!({"httpMethod": "POST", "path": "/orders"})),
    );
    let nodes = vec![
        handler_a.clone(),
        handler_b.clone(),
        route_a.clone(),
        route_b.clone(),
    ];
    let edges = vec![
        Edge {
            src: handler_a.id.clone(),
            dst: route_a.id.clone(),
            kind: EdgeKind::HandlesRoute,
            confidence: 1.0,
            reason: String::new(),
                props: None,
        },
        Edge {
            src: handler_b.id.clone(),
            dst: route_b.id.clone(),
            kind: EdgeKind::HandlesRoute,
            confidence: 1.0,
            reason: String::new(),
                props: None,
        },
    ];
    let community_edges = vec![
        Edge {
            src: handler_a.id.clone(),
            dst: comm_a.id.clone(),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
                props: None,
        },
        Edge {
            src: handler_b.id.clone(),
            dst: comm_b.id.clone(),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
                props: None,
        },
    ];
    let graph = WikiGraph::build(&nodes, &edges, &[comm_a, comm_b], &community_edges);
    let evidence = build_feature_evidence(
        &["Community:0".to_string(), "Community:1".to_string()],
        &graph,
        &std::env::temp_dir(),
        &EvidenceCorpus::default(),
    );

    assert!(evidence.contains("[C1-R1] GET /orders"));
    assert!(evidence.contains("[C2-R1] POST /orders"));
    assert!(!evidence.contains("\n[R1]"));
}

#[test]
fn enrich_prompt_contains_community_name_and_routes() {
    let prompt = build_enrich_prompt(
        "order-service",
        "[R1] GET /api/orders\n[D1] Called by: payment-service; calls into: notification-service",
    );
    assert!(prompt.contains("order-service"));
    assert!(prompt.contains("GET /api/orders"));
    assert!(prompt.contains("payment-service"));
    assert!(prompt.contains("notification-service"));
}

#[test]
fn parse_llm_summary_errors_on_malformed_response() {
    let result = parse_llm_summary("Not JSON at all");
    assert!(result.is_err(), "malformed response should return Err");
}

#[test]
fn parse_llm_summary_errors_on_empty_json_fields() {
    let result = parse_llm_summary(r#"{"po": "", "ba": "", "dev": ""}"#);
    assert!(result.is_err(), "empty response should return Err");
}

#[test]
fn parse_llm_summary_extracts_valid_json() {
    let text = r#"{"po": "Business stuff", "ba": "Flow stuff", "dev": "Tech stuff"}"#;
    let result = parse_llm_summary(text).unwrap();
    assert_eq!(result.po, "Business stuff");
    assert_eq!(result.ba, "Flow stuff");
    assert_eq!(result.dev, "Tech stuff");
}

#[test]
fn parse_llm_summary_handles_json_in_markdown_block() {
    let text =
        "Here is the summary:\n```json\n{\"po\": \"A\", \"ba\": \"B\", \"dev\": \"C\"}\n```";
    let result = parse_llm_summary(text).unwrap();
    assert_eq!(result.po, "A");
}
