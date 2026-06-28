use std::collections::BTreeMap;

use crate::confidence::CONTRACT_HTTP_CLIENT;

use cih_core::{
    external_endpoint_id, kafka_topic_id, ContractKind, Edge, EdgeKind, Node, NodeKind, ParsedFile,
};

/// Convert parser-discovered inter-service contract sites into graph nodes and edges.
pub fn resolve_contract_edges(parsed: &[ParsedFile]) -> (Vec<Node>, Vec<Edge>) {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    for pf in parsed {
        for site in &pf.contract_sites {
            match &site.kind {
                ContractKind::HttpCall | ContractKind::HttpClientProxy => {
                    let Some(url_template) = site.url_template.as_deref() else {
                        continue;
                    };
                    let Some(http_method) = site.http_method.as_deref() else {
                        continue;
                    };
                    let method = http_method.to_ascii_uppercase();
                    let id = external_endpoint_id(&method, url_template);
                    let name = format!("{method} {url_template}");
                    let source = match &site.kind {
                        ContractKind::HttpClientProxy => "http-client-proxy",
                        _ => "http-client",
                    };
                    nodes.push(Node {
                        id: id.clone(),
                        kind: NodeKind::ExternalEndpoint,
                        name: name.clone(),
                        qualified_name: Some(name),
                        file: pf.file.clone(),
                        range: site.range,
                        props: Some(serde_json::json!({
                            "httpMethod": method,
                            "path": url_template,
                            "urlTemplate": url_template,
                            "source": source,
                        })),
                    });
                    edges.push(Edge {
                        src: site.in_callable.clone(),
                        dst: id,
                        kind: EdgeKind::ExternalCall,
                        confidence: CONTRACT_HTTP_CLIENT,
                        reason: match &site.kind {
                            ContractKind::HttpClientProxy => "http-client-proxy",
                            _ => "http-client",
                        }
                        .to_string(),
                        props: None,
                    });
                }
                ContractKind::EventPublish | ContractKind::EventListen => {
                    let Some(topic) = site.topic.as_deref() else {
                        continue;
                    };
                    let id = kafka_topic_id(topic);
                    nodes.push(Node {
                        id: id.clone(),
                        kind: NodeKind::KafkaTopic,
                        name: topic.to_string(),
                        qualified_name: Some(topic.to_string()),
                        file: pf.file.clone(),
                        range: site.range,
                        props: Some(serde_json::json!({
                            "topic": topic,
                        })),
                    });
                    let (kind, reason) = match &site.kind {
                        ContractKind::EventPublish => (EdgeKind::PublishesEvent, "event-publish"),
                        ContractKind::EventListen => (EdgeKind::ListensTo, "event-listen"),
                        _ => unreachable!("HTTP contract kind handled above"),
                    };
                    edges.push(Edge {
                        src: site.in_callable.clone(),
                        dst: id,
                        kind,
                        confidence: 0.8,
                        reason: reason.to_string(),
                        props: None,
                    });
                }
                ContractKind::Custom(_) => continue,
            }
        }
    }

    let mut deduped_nodes = BTreeMap::new();
    for node in nodes {
        deduped_nodes
            .entry(node.id.as_str().to_string())
            .or_insert(node);
    }
    let mut deduped_edges = BTreeMap::new();
    for edge in edges {
        let key = (
            edge.src.as_str().to_string(),
            edge.dst.as_str().to_string(),
            edge.kind.cypher_label(),
        );
        deduped_edges.entry(key).or_insert(edge);
    }

    (
        deduped_nodes.into_values().collect(),
        deduped_edges.into_values().collect(),
    )
}
