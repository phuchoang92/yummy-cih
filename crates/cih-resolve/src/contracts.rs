use std::collections::BTreeMap;
use std::path::Path;

use crate::confidence::{CONTRACT_HTTP_CLIENT, CONTRACT_HTTP_CLIENT_DYNAMIC};

use cih_core::{
    external_endpoint_id, kafka_topic_id, ContractKind, ContractSite, Edge, EdgeKind, Node,
    NodeKind, ParsedFile, UrlPart,
};
use cih_lang::{ConstantResolver, ResolutionContext};

/// Internal marker for an unresolved URL part; any path segment containing it
/// becomes `{*}` wholesale before emission (never a partial `v{*}`).
const UNRESOLVED: char = '\u{0}';

/// Convert parser-discovered inter-service contract sites into graph nodes and edges.
/// `resolver` folds `ConstRef` URL parts through the cross-file constant index;
/// unresolved refs and `Dynamic` parts degrade to `{*}` wildcards.
pub fn resolve_contract_edges(
    parsed: &[ParsedFile],
    resolver: &dyn ConstantResolver,
) -> (Vec<Node>, Vec<Edge>) {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    for pf in parsed {
        for site in &pf.contract_sites {
            match &site.kind {
                ContractKind::HttpCall | ContractKind::HttpClientProxy => {
                    let (url_template, dynamic) = match site.url_template.as_deref() {
                        Some(url) => (url.to_string(), false),
                        None => {
                            let Some(folded) = fold_http_url(site, pf, resolver) else {
                                continue;
                            };
                            (folded, true)
                        }
                    };
                    let Some(http_method) = site.http_method.as_deref() else {
                        continue;
                    };
                    let method = http_method.to_ascii_uppercase();
                    let id = external_endpoint_id(&method, &url_template);
                    let name = format!("{method} {url_template}");
                    let source = match &site.kind {
                        ContractKind::HttpClientProxy => "http-client-proxy",
                        _ => "http-client",
                    };
                    let mut props = serde_json::json!({
                        "httpMethod": method,
                        "path": url_template,
                        "urlTemplate": url_template,
                        "source": source,
                    });
                    if dynamic {
                        props["dynamic"] = serde_json::Value::Bool(true);
                    }
                    nodes.push(Node {
                        id: id.clone(),
                        kind: NodeKind::ExternalEndpoint,
                        name: name.clone(),
                        qualified_name: Some(name),
                        file: pf.file.clone(),
                        range: site.range,
                        props: Some(props),
                    });
                    edges.push(Edge {
                        src: site.in_callable.clone(),
                        dst: id,
                        kind: EdgeKind::ExternalCall,
                        confidence: if dynamic {
                            CONTRACT_HTTP_CLIENT_DYNAMIC
                        } else {
                            CONTRACT_HTTP_CLIENT
                        },
                        reason: match &site.kind {
                            ContractKind::HttpClientProxy => "http-client-proxy",
                            _ => "http-client",
                        }
                        .to_string(),
                        props: None,
                    });
                }
                ContractKind::EventPublish | ContractKind::EventListen => {
                    let topic = match site.topic.as_deref() {
                        Some(topic) => topic.to_string(),
                        // A dynamic topic must fold to a full literal — topics
                        // match by exact string, so a `{*}` topic is useless.
                        None => match fold_literal_topic(site, pf, resolver) {
                            Some(topic) => topic,
                            None => continue,
                        },
                    };
                    let topic = topic.as_str();
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
                        // Carry the messaging framework as structured data so cross-repo
                        // consumers classify Kafka vs Spring without guessing from `reason`.
                        props: site
                            .messaging_framework
                            .map(|fw| serde_json::json!({ "messaging_framework": fw })),
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

/// Fold a site's `url_parts` into a normalized path with `{*}` wildcards for
/// unresolved segments. `None` when there are no parts or the result carries
/// no information (`/` or all-wildcard).
fn fold_http_url(
    site: &ContractSite,
    pf: &ParsedFile,
    resolver: &dyn ConstantResolver,
) -> Option<String> {
    let raw = fold_parts_raw(site, pf, resolver)?;
    let normalized = cih_lang::normalize_external_url(&raw);
    let segments: Vec<String> = normalized
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            if segment.contains(UNRESOLVED) {
                "{*}".to_string()
            } else {
                segment.to_string()
            }
        })
        .collect();
    if segments.is_empty() || segments.iter().all(|segment| segment == "{*}") {
        return None;
    }
    Some(format!("/{}", segments.join("/")))
}

/// Fold a dynamic topic; only a fully-resolved literal is usable.
fn fold_literal_topic(
    site: &ContractSite,
    pf: &ParsedFile,
    resolver: &dyn ConstantResolver,
) -> Option<String> {
    let raw = fold_parts_raw(site, pf, resolver)?;
    (!raw.is_empty() && !raw.contains(UNRESOLVED)).then_some(raw)
}

/// Concatenate the parts, resolving `ConstRef`s via the constant index in the
/// site's own scope (owner class from `in_callable`, the file's imports).
/// Unresolved refs and `Dynamic` parts become the `UNRESOLVED` marker.
fn fold_parts_raw(
    site: &ContractSite,
    pf: &ParsedFile,
    resolver: &dyn ConstantResolver,
) -> Option<String> {
    let parts = site.url_parts.as_ref()?;
    if parts.is_empty() {
        return None;
    }
    let owner = owner_fqcn_of(site.in_callable.as_str());
    let ctx = ResolutionContext {
        file: Path::new(&pf.file),
        owner_fqcn: owner,
        imports: &pf.imports,
    };
    let mut out = String::new();
    for part in parts {
        match part {
            UrlPart::Lit(lit) => out.push_str(lit),
            UrlPart::ConstRef(name) => match resolver.resolve(name, &ctx) {
                Some(value) => out.push_str(&value),
                None => out.push(UNRESOLVED),
            },
            UrlPart::Dynamic => out.push(UNRESOLVED),
        }
    }
    Some(out)
}

/// `Method:pkg.Cls#m/2` → `pkg.Cls`; `Function:module#f/1` → `module`.
fn owner_fqcn_of(in_callable: &str) -> &str {
    let qualified = in_callable
        .split_once(':')
        .map(|(_, rest)| rest)
        .unwrap_or(in_callable);
    qualified.split('#').next().unwrap_or(qualified)
}
