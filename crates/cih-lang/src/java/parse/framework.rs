use cih_core::{
    ContractKind, ContractSite, Edge, EdgeKind, MessagingFramework, Node, NodeId, NodeKind,
    RouteSource,
};
use tree_sitter::Node as TsNode;

use super::{
    FileBuilder, annotation_name, annotation_string_values, annotations, base_type_simple,
    callable_context_at, first_argument_string_literal, first_constructor_argument_type,
    first_string_argument, infer_webclient_http_method, method_declarations, method_routes,
    normalize_external_url, normalize_route_path, param_type_names, range_of, receiver_has_type,
    rest_template_http_method, root_receiver_has_type, spring_method_routes_inner, text,
    type_context_at, url_argument_parts,
};

pub(super) fn collect_method_routes(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    if node.kind() == "method_declaration" {
        emit_method_routes_for_method(node, src, builder);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_method_routes(child, src, builder);
    }
}

fn emit_method_routes_for_method(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    let routes = method_routes(node, src);
    if routes.is_empty() {
        return;
    }

    let Some(callable) = callable_context_at(node.start_byte(), builder).cloned() else {
        return;
    };
    let prefix = type_context_at(node.start_byte(), builder)
        .and_then(|ctx| ctx.spring_prefix.clone())
        .filter(|p| !p.is_empty())
        .unwrap_or_default();

    for route in routes {
        let path = normalize_route_path(&route.path, &prefix);
        let name = format!("{} {path}", route.http_method);
        let route_id = NodeId::new(format!("Route:{name}"));
        let reason = match route.source {
            RouteSource::SpringMvc => format!(
                "spring-{}",
                route.annotations.first().map(String::as_str).unwrap_or("")
            ),
            RouteSource::JaxRs => format!("jaxrs-{}", route.http_method),
            _ => format!("{:?}-{}", route.source, route.http_method),
        };
        builder.nodes.push(Node {
            id: route_id.clone(),
            kind: NodeKind::Route,
            name: name.clone(),
            qualified_name: Some(name),
            file: builder.file.clone(),
            range: route.range,
            props: Some(serde_json::json!({
                "httpMethod": route.http_method,
                "path": path,
                "route_annotations": route.annotations,
                "source": route.source,
                "handler": callable.in_fqcn,
            })),
        });
        builder.edges.push(Edge {
            src: callable.id.clone(),
            dst: route_id,
            kind: EdgeKind::HandlesRoute,
            confidence: 1.0,
            reason,
            props: None,
        });
    }
}

pub(super) fn collect_contract_sites(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    match node.kind() {
        "interface_declaration" => emit_feign_contracts(node, src, builder),
        "method_declaration" => emit_listener_contracts(node, src, builder),
        "method_invocation" => emit_invocation_contract(node, src, builder),
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_contract_sites(child, src, builder);
    }
}

fn emit_feign_contracts(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    let Some(feign) = annotations(node)
        .into_iter()
        .find(|ann| annotation_name(*ann, src).as_deref() == Some("FeignClient"))
    else {
        return;
    };
    let base = annotation_string_values(feign, src, &["url", "path", "value"])
        .into_iter()
        .next();

    for method in method_declarations(node) {
        let Some(callable) = callable_context_at(method.start_byte(), builder).cloned() else {
            continue;
        };
        for route in spring_method_routes_inner(method, src) {
            let url = if let Some(base) = base.as_deref().filter(|base| base.starts_with('/')) {
                normalize_route_path(&route.path, base)
            } else {
                normalize_external_url(&route.path)
            };
            builder.contract_sites.push(ContractSite {
                kind: ContractKind::HttpClientProxy,
                url_template: Some(url),
                topic: None,
                http_method: Some(route.http_method.to_string()),
                messaging_framework: None,
                url_parts: None,
                in_callable: callable.id.clone(),
                range: route.range,
            });
        }
    }
}

fn emit_listener_contracts(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    let Some(callable) = callable_context_at(node.start_byte(), builder).cloned() else {
        return;
    };
    for annotation in annotations(node) {
        match annotation_name(annotation, src).as_deref() {
            Some("KafkaListener") => {
                for topic in
                    annotation_string_values(annotation, src, &["topics", "topic", "value"])
                {
                    builder.contract_sites.push(ContractSite {
                        kind: ContractKind::EventListen,
                        url_template: None,
                        topic: Some(topic),
                        http_method: None,
                        messaging_framework: Some(MessagingFramework::Kafka),
                        url_parts: None,
                        in_callable: callable.id.clone(),
                        range: range_of(annotation),
                    });
                }
            }
            Some("EventListener") => {
                if let Some(topic) = param_type_names(node, src).into_iter().next() {
                    builder.contract_sites.push(ContractSite {
                        kind: ContractKind::EventListen,
                        url_template: None,
                        topic: Some(base_type_simple(&topic)),
                        http_method: None,
                        messaging_framework: Some(MessagingFramework::Spring),
                        url_parts: None,
                        in_callable: callable.id.clone(),
                        range: range_of(annotation),
                    });
                }
            }
            _ => {}
        }
    }
}

fn emit_invocation_contract(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let method = text(name_node, src);
    let Some(callable) = callable_context_at(node.start_byte(), builder).cloned() else {
        return;
    };
    let receiver = node
        .child_by_field_name("object")
        .map(|object| text(object, src))
        .unwrap_or_default();

    if let Some(http_method) = rest_template_http_method(&method) {
        if receiver_has_type(builder, &callable.in_fqcn, &receiver, "RestTemplate") {
            let url_template =
                first_string_argument(node, src).map(|url| normalize_external_url(&url));
            let url_parts = if url_template.is_none() {
                url_argument_parts(node, src)
            } else {
                None
            };
            builder.contract_sites.push(ContractSite {
                kind: ContractKind::HttpCall,
                url_template,
                topic: None,
                http_method: Some(http_method.to_string()),
                messaging_framework: None,
                url_parts,
                in_callable: callable.id,
                range: range_of(node),
            });
        }
        return;
    }

    if method == "uri" {
        if let Some(http_method) = infer_webclient_http_method(&receiver) {
            if root_receiver_has_type(builder, &callable.in_fqcn, &receiver, "WebClient") {
                let url_template =
                    first_string_argument(node, src).map(|url| normalize_external_url(&url));
                let url_parts = if url_template.is_none() {
                    url_argument_parts(node, src)
                } else {
                    None
                };
                builder.contract_sites.push(ContractSite {
                    kind: ContractKind::HttpCall,
                    url_template,
                    topic: None,
                    http_method: Some(http_method.to_string()),
                    messaging_framework: None,
                    url_parts,
                    in_callable: callable.id,
                    range: range_of(node),
                });
            }
        }
        return;
    }

    if method == "send" && receiver_has_type(builder, &callable.in_fqcn, &receiver, "KafkaTemplate")
    {
        // Topic is positional arg 0; scanning the whole list would read a
        // literal payload as the topic when the topic is a constant.
        let topic = first_argument_string_literal(node, src);
        let url_parts = if topic.is_none() {
            url_argument_parts(node, src)
        } else {
            None
        };
        if topic.is_some() || url_parts.is_some() {
            builder.contract_sites.push(ContractSite {
                kind: ContractKind::EventPublish,
                url_template: None,
                topic,
                http_method: None,
                messaging_framework: Some(MessagingFramework::Kafka),
                url_parts,
                in_callable: callable.id,
                range: range_of(node),
            });
        }
        return;
    }

    if method == "publishEvent"
        && receiver_has_type(
            builder,
            &callable.in_fqcn,
            &receiver,
            "ApplicationEventPublisher",
        )
    {
        if let Some(topic) = first_constructor_argument_type(node, src) {
            builder.contract_sites.push(ContractSite {
                kind: ContractKind::EventPublish,
                url_template: None,
                topic: Some(topic),
                http_method: None,
                messaging_framework: Some(MessagingFramework::Spring),
                url_parts: None,
                in_callable: callable.id,
                range: range_of(node),
            });
        }
    }
}
