mod cache;
mod class_enrich;
mod community_enrich;
mod config;
mod feature_enrich;
mod flow_enrich;
mod loader;
mod run;

pub use class_enrich::enrich_classes_for_chains;
pub use config::WikiConfig;
pub use crate::llm::LlmCallConfig;
pub use feature_enrich::{
    build_feature_evidence, build_feature_user_prompt, cached_feature_summary, enrich_one_feature,
    parse_feature_summary, retain_matching_feature_groups,
};
pub use flow_enrich::parse_flow_summary;
pub use loader::community_matches_route_prefix;
pub use run::run_wiki;

#[cfg(test)]
mod tests {
    use super::flow_enrich::{enrich_route_flows, parse_flow_summary};
    use super::config::fnv64;
    use crate::llm::{LlmRequest, LlmResponse};
    use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};
    use cih_wiki::{FlowCacheEntry, WikiGraph};
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::{
        atomic::{AtomicUsize, Ordering as AOrdering},
        Mutex,
    };
    use anyhow::Result;

    struct MockLlm {
        responses: Mutex<VecDeque<Result<String>>>,
        pub calls: AtomicUsize,
    }
    impl MockLlm {
        fn new(responses: Vec<Result<String>>) -> Self {
            Self { responses: Mutex::new(responses.into()), calls: AtomicUsize::new(0) }
        }
    }
    impl crate::llm::LlmAdapter for MockLlm {
        fn call(&self, _key: Option<&str>, _req: &LlmRequest) -> Result<LlmResponse> {
            self.calls.fetch_add(1, AOrdering::SeqCst);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no more mock responses")))
                .map(|text| LlmResponse { text })
        }
    }

    fn node(id: &str, kind: NodeKind, name: &str) -> Node {
        Node {
            id: NodeId::new(id.to_string()),
            kind,
            name: name.to_string(),
            qualified_name: None,
            file: "com/example/modules/orders/OrderController.java".to_string(),
            range: Range::default(),
            props: None,
        }
    }

    fn edge(src: &str, dst: &str, kind: EdgeKind) -> Edge {
        Edge {
            src: NodeId::new(src.to_string()),
            dst: NodeId::new(dst.to_string()),
            kind,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        }
    }

    fn flow_json(narrative: &str) -> String {
        format!(
            r#"{{"narrative": "{narrative}", "business_impact": "Important.", "step_descriptions": ["Queries the service"]}}"#
        )
    }

    #[test]
    fn flow_cache_hit_skips_llm_on_second_call() {
        let handler_id = "Method:com.example.modules.orders.OrderController#list/0";
        let ctrl_cls = "Class:com.example.modules.orders.OrderController";
        let service_id = "Method:com.example.modules.orders.OrderService#findAll/0";
        let svc_cls = "Class:com.example.modules.orders.OrderService";
        let route_id = "Route:GET:/orders";

        let nodes = vec![
            node(ctrl_cls, NodeKind::Class, "OrderController"),
            node(handler_id, NodeKind::Method, "list"),
            node(svc_cls, NodeKind::Class, "OrderService"),
            node(service_id, NodeKind::Method, "findAll"),
            node(route_id, NodeKind::Route, "GET /orders"),
        ];
        let edges = vec![
            edge(ctrl_cls, handler_id, EdgeKind::HasMethod),
            edge(svc_cls, service_id, EdgeKind::HasMethod),
            edge(handler_id, route_id, EdgeKind::HandlesRoute),
            edge(handler_id, service_id, EdgeKind::Calls),
        ];
        let graph = WikiGraph::build(&nodes, &edges, &[], &[]);

        let flow_response = flow_json("Lists all orders for the customer.");

        let adapter1 = MockLlm::new(vec![Ok(flow_response.clone())]);
        let empty_cache = BTreeMap::new();
        let pool = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let (summaries1, updates1) = enrich_route_flows(
            &graph, None, &adapter1, None, "model", 1000, 30, 0, "en", false,
            &empty_cache, &pool,
        );
        assert_eq!(adapter1.calls.load(AOrdering::SeqCst), 1, "first run must call LLM");
        assert!(summaries1.contains_key(handler_id));
        assert_eq!(updates1.len(), 1);

        let mut flow_cache: BTreeMap<String, FlowCacheEntry> = BTreeMap::new();
        for (id, ev_hash, summary) in updates1 {
            flow_cache.insert(id, FlowCacheEntry { evidence_hash: ev_hash, summary });
        }

        let adapter2 = MockLlm::new(vec![]);
        let (summaries2, updates2) = enrich_route_flows(
            &graph, None, &adapter2, None, "model", 1000, 30, 0, "en", false,
            &flow_cache, &pool,
        );
        assert_eq!(adapter2.calls.load(AOrdering::SeqCst), 0, "second run must hit cache");
        assert!(summaries2.contains_key(handler_id));
        assert_eq!(summaries2[handler_id].narrative, summaries1[handler_id].narrative);
        assert!(updates2.is_empty(), "cache hit must not produce new updates");
    }

    #[test]
    fn flow_cache_miss_on_changed_call_chain() {
        let handler_id = "Method:com.example.modules.orders.OrderController#list/0";
        let ctrl_cls = "Class:com.example.modules.orders.OrderController";
        let service_id = "Method:com.example.modules.orders.OrderService#findAll/0";
        let svc_cls = "Class:com.example.modules.orders.OrderService";
        let extra_id = "Method:com.example.modules.orders.OrderRepo#count/0";
        let repo_cls = "Class:com.example.modules.orders.OrderRepo";
        let route_id = "Route:GET:/orders";

        let nodes_v1 = vec![
            node(ctrl_cls, NodeKind::Class, "OrderController"),
            node(handler_id, NodeKind::Method, "list"),
            node(svc_cls, NodeKind::Class, "OrderService"),
            node(service_id, NodeKind::Method, "findAll"),
            node(route_id, NodeKind::Route, "GET /orders"),
        ];
        let edges_v1 = vec![
            edge(ctrl_cls, handler_id, EdgeKind::HasMethod),
            edge(svc_cls, service_id, EdgeKind::HasMethod),
            edge(handler_id, route_id, EdgeKind::HandlesRoute),
            edge(handler_id, service_id, EdgeKind::Calls),
        ];
        let graph_v1 = WikiGraph::build(&nodes_v1, &edges_v1, &[], &[]);

        let adapter1 = MockLlm::new(vec![Ok(flow_json("Lists orders."))]);
        let empty_cache = BTreeMap::new();
        let pool = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let (_, updates1) = enrich_route_flows(
            &graph_v1, None, &adapter1, None, "model", 1000, 30, 0, "en", false,
            &empty_cache, &pool,
        );
        let mut flow_cache: BTreeMap<String, FlowCacheEntry> = BTreeMap::new();
        for (id, ev_hash, summary) in updates1 {
            flow_cache.insert(id, FlowCacheEntry { evidence_hash: ev_hash, summary });
        }

        let nodes_v2 = vec![
            node(ctrl_cls, NodeKind::Class, "OrderController"),
            node(handler_id, NodeKind::Method, "list"),
            node(svc_cls, NodeKind::Class, "OrderService"),
            node(service_id, NodeKind::Method, "findAll"),
            node(repo_cls, NodeKind::Class, "OrderRepo"),
            node(extra_id, NodeKind::Method, "count"),
            node(route_id, NodeKind::Route, "GET /orders"),
        ];
        let edges_v2 = vec![
            edge(ctrl_cls, handler_id, EdgeKind::HasMethod),
            edge(svc_cls, service_id, EdgeKind::HasMethod),
            edge(repo_cls, extra_id, EdgeKind::HasMethod),
            edge(handler_id, route_id, EdgeKind::HandlesRoute),
            edge(handler_id, service_id, EdgeKind::Calls),
            edge(service_id, extra_id, EdgeKind::Calls),
        ];
        let graph_v2 = WikiGraph::build(&nodes_v2, &edges_v2, &[], &[]);

        let adapter2 = MockLlm::new(vec![Ok(flow_json("Lists orders with count."))]);
        let (summaries2, _) = enrich_route_flows(
            &graph_v2, None, &adapter2, None, "model", 1000, 30, 0, "en", false,
            &flow_cache, &pool,
        );
        assert_eq!(adapter2.calls.load(AOrdering::SeqCst), 1, "cache miss must call LLM");
        assert!(summaries2.contains_key(handler_id));
    }
}
