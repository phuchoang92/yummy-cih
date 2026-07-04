//! Phase 1b — best-effort integration XML extraction.
//!
//! Reads Camel, Blueprint, Spring XML and CXF config files and emits
//! `IntegrationRoute` / `MessageDestination` nodes plus wiring edges. This is a
//! deliberately lightweight text scanner — we do not pull in an XML parser
//! dependency. Malformed input simply yields fewer facts; it never panics.

use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};

pub struct IntegrationXmlOutput {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// Detect if a file is an integration XML by checking its content for known namespaces.
fn is_integration_xml(content: &str) -> Option<&'static str> {
    if content.contains("http://camel.apache.org/schema/spring")
        || content.contains("http://camel.apache.org/schema/blueprint")
    {
        return Some("camel");
    }
    if content.contains("http://www.osgi.org/xmlns/blueprint") {
        return Some("blueprint");
    }
    if content.contains("http://www.springframework.org/schema/beans") && content.contains("<bean")
    {
        return Some("spring");
    }
    if content.contains("http://cxf.apache.org/") {
        return Some("cxf");
    }
    None
}

/// Extract URI scheme and component name from a Camel endpoint URI.
/// "jms:queue:my-queue" → ("jms", "my-queue")
/// "direct:my-route" → ("direct", "my-route")
#[doc(hidden)]
pub fn parse_camel_uri(uri: &str) -> (&str, &str) {
    let scheme = uri.split(':').next().unwrap_or("");
    let rest = if uri.len() > scheme.len() + 1 {
        let after_scheme = &uri[scheme.len() + 1..];
        // strip queue/topic prefix for jms URIs
        if let Some(stripped) = after_scheme
            .strip_prefix("queue:")
            .or_else(|| after_scheme.strip_prefix("topic:"))
        {
            stripped
        } else {
            after_scheme
        }
    } else {
        ""
    };
    let name = rest.split('?').next().unwrap_or(rest);
    (scheme, name)
}

/// Returns true if this is an in-process Camel scheme (no external broker).
fn is_internal_scheme(scheme: &str) -> bool {
    matches!(scheme, "direct" | "seda" | "vm" | "direct-vm" | "stub")
}

/// Returns true if this is a messaging broker scheme.
fn is_message_scheme(scheme: &str) -> bool {
    matches!(
        scheme,
        "jms" | "activemq"
            | "kafka"
            | "amqp"
            | "rabbitmq"
            | "artemis"
            | "ibmmq"
            | "aws-sqs"
            | "aws-sns"
    )
}

pub fn extract_integration_xml(rel_path: &str, content: &str) -> IntegrationXmlOutput {
    let kind = match is_integration_xml(content) {
        Some(k) => k,
        None => {
            return IntegrationXmlOutput {
                nodes: vec![],
                edges: vec![],
            }
        }
    };

    match kind {
        "camel" => extract_camel_xml(rel_path, content),
        "blueprint" => extract_blueprint_xml(rel_path, content),
        "spring" => extract_spring_beans_xml(rel_path, content),
        _ => IntegrationXmlOutput {
            nodes: vec![],
            edges: vec![],
        },
    }
}

fn extract_camel_xml(rel_path: &str, content: &str) -> IntegrationXmlOutput {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    // Simple XML text scanning — we don't pull in an XML parser dependency.
    // Find <route id="..."> ... <from uri="..."/> ... <to uri="..."/> patterns
    // This is a best-effort scanner, not a full XML parser.

    let mut from_uris: Vec<(String, usize)> = Vec::new(); // (uri, char_pos)
    let mut to_uris: Vec<(String, usize)> = Vec::new();
    let mut route_ids: Vec<(String, usize)> = Vec::new();

    // Scan for route elements
    let mut i = 0;
    let bytes = content.as_bytes();
    while i < bytes.len() {
        // Look for <route or <from or <to
        if bytes[i] == b'<' {
            let tag_start = i;
            i += 1;
            // skip whitespace
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }

            // Read tag name
            let name_start = i;
            while i < bytes.len()
                && !bytes[i].is_ascii_whitespace()
                && bytes[i] != b'>'
                && bytes[i] != b'/'
            {
                i += 1;
            }
            let tag_name = &content[name_start..i];

            match tag_name {
                "route" | "camelContext" => {
                    // extract id attribute
                    if let Some(id) = extract_xml_attr(&content[tag_start..], "id") {
                        route_ids.push((id, tag_start));
                    }
                }
                "from" => {
                    if let Some(uri) = extract_xml_attr(&content[tag_start..], "uri") {
                        from_uris.push((uri, tag_start));
                    }
                }
                "to" | "toD" => {
                    if let Some(uri) = extract_xml_attr(&content[tag_start..], "uri") {
                        to_uris.push((uri, tag_start));
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }

    // Emit IntegrationRoute nodes for each from-URI
    let mut from_node_ids: Vec<NodeId> = Vec::new();
    for (uri, _pos) in &from_uris {
        let (scheme, name) = parse_camel_uri(uri);
        let route_slug = format!("{}-{}", scheme, name.replace(['/', ':'], "-"));
        let node_id = cih_core::integration_route_id(rel_path, &route_slug);

        nodes.push(Node {
            id: node_id.clone(),
            kind: NodeKind::IntegrationRoute,
            name: uri.clone(),
            qualified_name: Some(format!("{rel_path}#{uri}")),
            file: rel_path.to_string(),
            range: Range::default(),
            props: Some(serde_json::json!({
                "uri": uri,
                "scheme": scheme,
                "source": "camel_xml",
            })),
        });
        from_node_ids.push(node_id);
    }

    // Emit MessageDestination nodes and edges for external messaging to-URIs
    for (uri, _pos) in &to_uris {
        let (scheme, name) = parse_camel_uri(uri);

        if is_internal_scheme(scheme) {
            // direct/seda/vm: emit IntegrationLink to the target route (if it exists)
            // For now emit a placeholder IntegrationRoute node for the target
            let target_id = cih_core::integration_route_id("direct", name);
            if !from_node_ids.is_empty() {
                edges.push(Edge {
                    src: from_node_ids[0].clone(),
                    dst: target_id,
                    kind: EdgeKind::IntegrationLink,
                    confidence: 0.8,
                    reason: format!("camel-{scheme}"),
            props: None,
                });
            }
        } else if is_message_scheme(scheme) {
            // External broker: emit MessageDestination node
            let dest_id = cih_core::message_destination_id(scheme, name);

            if !nodes.iter().any(|n| n.id == dest_id) {
                nodes.push(Node {
                    id: dest_id.clone(),
                    kind: NodeKind::MessageDestination,
                    name: name.to_string(),
                    qualified_name: Some(format!("{scheme}:{name}")),
                    file: rel_path.to_string(),
                    range: Range::default(),
                    props: Some(serde_json::json!({
                        "destination_type": scheme,
                        "component": scheme,
                        "uri": uri,
                    })),
                });
            }

            if !from_node_ids.is_empty() {
                edges.push(Edge {
                    src: from_node_ids[0].clone(),
                    dst: dest_id,
                    kind: EdgeKind::PublishesEvent,
                    confidence: 0.9,
                    reason: format!("camel-{scheme}-to"),
            props: None,
                });
            }
        }
    }

    IntegrationXmlOutput { nodes, edges }
}

fn extract_blueprint_xml(rel_path: &str, content: &str) -> IntegrationXmlOutput {
    // Blueprint XML: scan for <bean>, <reference>, <service> bindings
    // These create wiring facts between services.
    // For now emit IntegrationRoute nodes for <service> and edges for <reference>
    let mut nodes = Vec::new();
    let edges = Vec::new();

    // Scan for <service interface="..." ref="...">
    let mut i = 0;
    let bytes = content.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let tag_start = i;
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let name_start = i;
            while i < bytes.len()
                && !bytes[i].is_ascii_whitespace()
                && bytes[i] != b'>'
                && bytes[i] != b'/'
            {
                i += 1;
            }
            let tag_name = &content[name_start..i];

            if tag_name == "service" {
                let iface = extract_xml_attr(&content[tag_start..], "interface");
                let refer = extract_xml_attr(&content[tag_start..], "ref");
                if let (Some(iface), Some(refer)) = (iface, refer) {
                    let node_id = cih_core::integration_route_id(rel_path, &refer);
                    nodes.push(Node {
                        id: node_id,
                        kind: NodeKind::IntegrationRoute,
                        name: refer.clone(),
                        qualified_name: Some(format!("{rel_path}#service:{refer}")),
                        file: rel_path.to_string(),
                        range: Range::default(),
                        props: Some(serde_json::json!({
                            "interface": iface,
                            "ref": refer,
                            "source": "blueprint_xml",
                        })),
                    });
                }
            }
        }
        i += 1;
    }

    IntegrationXmlOutput { nodes, edges }
}

fn extract_spring_beans_xml(rel_path: &str, content: &str) -> IntegrationXmlOutput {
    // Spring XML beans: scan for <bean id="..." class="...">
    // These register service beans. Emit as IntegrationRoute nodes for wiring visibility.
    let mut nodes = Vec::new();
    let edges = Vec::new();

    let mut i = 0;
    let bytes = content.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let tag_start = i;
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let name_start = i;
            while i < bytes.len()
                && !bytes[i].is_ascii_whitespace()
                && bytes[i] != b'>'
                && bytes[i] != b'/'
            {
                i += 1;
            }
            let tag_name = &content[name_start..i];

            if tag_name == "bean" {
                let id = extract_xml_attr(&content[tag_start..], "id");
                let class = extract_xml_attr(&content[tag_start..], "class");
                if let (Some(id), Some(class)) = (id, class) {
                    let node_id = cih_core::integration_route_id(rel_path, &id);
                    nodes.push(Node {
                        id: node_id,
                        kind: NodeKind::IntegrationRoute,
                        name: id.clone(),
                        qualified_name: Some(class.to_string()),
                        file: rel_path.to_string(),
                        range: Range::default(),
                        props: Some(serde_json::json!({
                            "bean_id": id,
                            "class": class,
                            "source": "spring_xml",
                        })),
                    });
                }
            }
        }
        i += 1;
    }

    IntegrationXmlOutput { nodes, edges }
}

/// Extract a named XML attribute value from a tag fragment.
/// Handles both single and double quoted values.
fn extract_xml_attr(tag_fragment: &str, attr_name: &str) -> Option<String> {
    // Find attr_name= in the fragment (limited to first 2000 chars to avoid scanning too far)
    let search_in = &tag_fragment[..tag_fragment.len().min(2000)];
    let needle = format!("{attr_name}=");
    let pos = search_in.find(&needle)?;
    let after = &search_in[pos + needle.len()..];
    let first = after.chars().next()?;
    if first == '"' || first == '\'' {
        let end = after[1..].find(first)?;
        Some(after[1..end + 1].to_string())
    } else {
        None
    }
}

