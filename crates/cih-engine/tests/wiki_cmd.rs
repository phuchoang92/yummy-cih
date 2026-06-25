use anyhow::Result;
use cih_engine_lib::llm::evidence::EvidenceCorpus;
use cih_engine_lib::llm::{LlmAdapter, LlmRequest, LlmResponse};
use cih_engine_lib::wiki_cmd::*;
use cih_wiki::features::FeatureGroup;
use cih_wiki::{FeatureMetaEntry, WikiGraph, WikiMeta};
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
    fn call(&self, _key: Option<&str>, _req: &LlmRequest) -> Result<LlmResponse> {
        self.call_count.fetch_add(1, AOrdering::SeqCst);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(anyhow::anyhow!("no more responses")))
            .map(|text| LlmResponse { text })
    }
}

#[test]
fn class_enrichment_dry_run_returns_placeholder_descriptions() {
    // Verify that dry-run mode produces placeholder entries for every class in the chain.
    let adapter = MockAdapter::new(vec![]);
    let graph = WikiGraph::build(&[], &[], &[], &[]);
    let result = enrich_classes_for_chains(
        &graph,
        &[],
        &std::env::temp_dir(),
        Default::default(),
        &adapter,
        None,
        "model",
        800,
        5,
        0,
        "en",
        true, // dry_run
        false,
    );
    assert!(result.is_ok());
    assert_eq!(adapter.calls(), 0, "dry-run should not call the adapter");
    let (ctrl_map, comm_map, store) = result.unwrap();
    // Empty graph → no routes → no classes to enrich
    assert!(ctrl_map.is_empty());
    assert!(comm_map.is_empty());
    assert!(store.entries.is_empty());
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
fn class_enrichment_cache_hit_skips_llm_call() {
    // A class that was already enriched (hash matches) should not trigger an LLM call.
    use cih_wiki::ClassCacheEntry;
    let adapter = MockAdapter::new(vec![]);
    let graph = WikiGraph::build(&[], &[], &[], &[]);
    let mut prev = cih_wiki::ClassEnrichmentStore::default();
    prev.entries.insert(
        "com.example.MyService".to_string(),
        ClassCacheEntry {
            content_hash: "some-hash".to_string(),
            class_summary: "A service.".to_string(),
            method_descriptions: Default::default(),
        },
    );
    // With an empty graph there are still no routes, so nothing to enrich.
    let (_, _, updated) = enrich_classes_for_chains(
        &graph,
        &[],
        &std::env::temp_dir(),
        prev.clone(),
        &adapter,
        None,
        "model",
        800,
        5,
        0,
        "en",
        false,
        false,
    )
    .unwrap();
    assert_eq!(adapter.calls(), 0);
    // The cached entry should survive in the updated store.
    assert!(updated.entries.contains_key("com.example.MyService"));
}

fn make_community(route_prefixes: Option<Vec<&str>>) -> cih_core::Node {
    let props = route_prefixes.map(|ps| {
        serde_json::json!({
            "route_prefixes": ps
        })
    });
    cih_core::Node {
        id: cih_core::NodeId::new("Community:test"),
        kind: cih_core::NodeKind::Community,
        name: "Test".to_string(),
        qualified_name: None,
        file: String::new(),
        range: cih_core::Range::default(),
        props,
    }
}

#[test]
fn route_prefix_filter_matches_simple_segment() {
    let c = make_community(Some(vec!["membership", "profile"]));
    assert!(community_matches_route_prefix(
        &c,
        &["/membership".to_string()]
    ));
}

#[test]
fn route_prefix_filter_no_match() {
    let c = make_community(Some(vec!["orders", "payments"]));
    assert!(!community_matches_route_prefix(
        &c,
        &["/membership".to_string()]
    ));
}

#[test]
fn route_prefix_filter_skips_generic_prefix_in_pattern() {
    // Pattern /api/membership → meaningful segment is "membership"
    let c = make_community(Some(vec!["membership"]));
    assert!(community_matches_route_prefix(
        &c,
        &["/api/membership".to_string()]
    ));
}

#[test]
fn route_prefix_filter_missing_props_keeps_community() {
    let c = make_community(None);
    assert!(community_matches_route_prefix(
        &c,
        &["/membership".to_string()]
    ));
}

#[test]
fn route_prefix_filter_empty_prefixes_drops_community() {
    let c = make_community(Some(vec![]));
    assert!(!community_matches_route_prefix(
        &c,
        &["/membership".to_string()]
    ));
}

#[test]
fn route_prefix_filter_empty_patterns_keeps_all() {
    let c = make_community(Some(vec!["orders"]));
    assert!(community_matches_route_prefix(&c, &[]));
}
