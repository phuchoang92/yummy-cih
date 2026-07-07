//! Phase 1b — best-effort integration XML extraction.
//!
//! Reads Camel, Blueprint, Spring XML and CXF config files and emits
//! `IntegrationRoute` / `MessageDestination` nodes plus wiring edges. Spring/Blueprint/CXF
//! structured config is parsed with the namespace-aware `quick-xml` `NsReader` (so aliased
//! namespace prefixes, nested/inline elements, comments and entities are handled correctly);
//! Camel routing is still a lightweight URI-string scanner. Best-effort throughout: malformed
//! input simply yields fewer facts and never panics.

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

/// Remove `<!-- … -->` comment spans so commented-out config isn't parsed as live facts.
/// An unterminated comment drops the remainder. Borrows when there are no comments.
fn strip_xml_comments(content: &str) -> std::borrow::Cow<'_, str> {
    if !content.contains("<!--") {
        return std::borrow::Cow::Borrowed(content);
    }
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start + 4..].find("-->") {
            Some(end) => rest = &rest[start + 4 + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out)
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
        "jms"
            | "activemq"
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

    // Strip `<!-- … -->` first so commented-out config isn't parsed as live facts (the tag
    // scanners don't otherwise skip comments). Node ranges here are already `Range::default`.
    let stripped = strip_xml_comments(content);
    let content = stripped.as_ref();

    match kind {
        "camel" => extract_camel_xml(rel_path, content),
        "blueprint" => extract_structured_xml(rel_path, content, "blueprint_xml"),
        // A CXF Spring file may declare <jaxrs:server> beside (or instead of) <bean>
        // definitions, so route the "cxf" kind through the Spring/structured extractor too.
        "spring" | "cxf" => extract_structured_xml(rel_path, content, "spring_xml"),
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

/// Local accumulator for a `<jaxrs:server>` element while its body is streamed.
struct JaxrsServerAcc {
    address: String,
    server_id: Option<String>,
    beans: Vec<String>,
    bean_classes: Vec<String>,
    /// Nesting depth at which the `<server>` opened, to detect its matching close.
    depth: i32,
}

/// Parse Spring / Blueprint / CXF structured config with a namespace-aware pull parser.
///
/// Emits the same `IntegrationRoute` node shapes as the previous byte scanner — top-level
/// `<bean>` (source = `source_label`), blueprint `<service>`, `<jaxrs:server>` (source
/// `cxf_jaxrs_server`, carrying `beans` refs and inline `bean_classes`), and OSGi whiteboard
/// `<entry>` (source `osgi_servlet`). Matching `<server>` by namespace URI handles any prefix
/// alias; nested traversal captures inline service beans; comments/CDATA/entities are handled by
/// the parser. Best-effort: a parse error stops the walk with whatever was collected.
fn extract_structured_xml(
    rel_path: &str,
    content: &str,
    source_label: &str,
) -> IntegrationXmlOutput {
    use quick_xml::events::{BytesStart, Event};
    use quick_xml::name::{Namespace, ResolveResult};
    use quick_xml::reader::NsReader;

    // CXF JAX-RS namespaces (Spring and Blueprint variants) that bind a `<server>` element.
    const CXF_JAXRS_NS: [&[u8]; 2] = [
        b"http://cxf.apache.org/jaxrs",
        b"http://cxf.apache.org/blueprint/jaxrs",
    ];

    fn attr_val(e: &BytesStart, local: &[u8]) -> Option<String> {
        e.attributes().flatten().find_map(|a| {
            if a.key.local_name().as_ref() == local {
                a.unescape_value().ok().map(|v| v.into_owned())
            } else {
                None
            }
        })
    }

    let mut nodes = Vec::new();
    let mut reader = NsReader::from_str(content);
    let mut depth: i32 = 0;
    let mut server: Option<JaxrsServerAcc> = None;

    loop {
        match reader.read_resolved_event() {
            Ok((ns, ev @ (Event::Start(_) | Event::Empty(_)))) => {
                let is_start = matches!(ev, Event::Start(_));
                let e = match &ev {
                    Event::Start(e) | Event::Empty(e) => e,
                    _ => unreachable!(),
                };
                let is_server = matches!(ns, ResolveResult::Bound(Namespace(n)) if CXF_JAXRS_NS.contains(&n))
                    && e.local_name().as_ref() == b"server";

                if is_server {
                    if let Some(address) = attr_val(e, b"address") {
                        let acc = JaxrsServerAcc {
                            address,
                            server_id: attr_val(e, b"id"),
                            beans: Vec::new(),
                            bean_classes: Vec::new(),
                            depth,
                        };
                        if is_start {
                            server = Some(acc);
                        } else {
                            push_jaxrs_server_node(rel_path, acc, &mut nodes);
                        }
                    }
                } else if let Some(acc) = server.as_mut() {
                    // Inside a <jaxrs:server> block: collect refs and inline service beans.
                    match e.local_name().as_ref() {
                        b"ref" => {
                            if let Some(b) =
                                attr_val(e, b"bean").or_else(|| attr_val(e, b"component-id"))
                            {
                                acc.beans.push(b);
                            }
                        }
                        b"bean" => {
                            if let Some(c) = attr_val(e, b"class") {
                                acc.bean_classes.push(c);
                            }
                        }
                        _ => {}
                    }
                } else {
                    // Top-level structured elements.
                    match e.local_name().as_ref() {
                        b"bean" => {
                            if let (Some(id), Some(class)) =
                                (attr_val(e, b"id"), attr_val(e, b"class"))
                            {
                                // Blueprint namespaces the id to avoid colliding with a same-named
                                // <service> node on repo-wide dedup.
                                let key = if source_label == "blueprint_xml" {
                                    format!("bean:{id}")
                                } else {
                                    id.clone()
                                };
                                nodes.push(Node {
                                    id: cih_core::integration_route_id(rel_path, &key),
                                    kind: NodeKind::IntegrationRoute,
                                    name: id.clone(),
                                    qualified_name: Some(class.clone()),
                                    file: rel_path.to_string(),
                                    range: Range::default(),
                                    props: Some(serde_json::json!({
                                        "bean_id": id,
                                        "class": class,
                                        "source": source_label,
                                    })),
                                });
                            }
                        }
                        b"service" => {
                            if let (Some(iface), Some(refer)) =
                                (attr_val(e, b"interface"), attr_val(e, b"ref"))
                            {
                                nodes.push(Node {
                                    id: cih_core::integration_route_id(rel_path, &refer),
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
                        b"entry" => {
                            if attr_val(e, b"key").as_deref()
                                == Some("osgi.http.whiteboard.servlet.pattern")
                            {
                                if let Some(pattern) = attr_val(e, b"value") {
                                    nodes.push(Node {
                                        id: cih_core::integration_route_id(
                                            rel_path,
                                            &format!("osgi-servlet-{pattern}"),
                                        ),
                                        kind: NodeKind::IntegrationRoute,
                                        name: pattern.clone(),
                                        qualified_name: None,
                                        file: rel_path.to_string(),
                                        range: Range::default(),
                                        props: Some(serde_json::json!({
                                            "source": "osgi_servlet",
                                            "servlet_pattern": pattern,
                                        })),
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }

                if is_start {
                    depth += 1;
                }
            }
            Ok((_, Event::End(_))) => {
                depth -= 1;
                if server.as_ref().is_some_and(|acc| depth == acc.depth) {
                    let acc = server.take().unwrap();
                    push_jaxrs_server_node(rel_path, acc, &mut nodes);
                }
            }
            Ok((_, Event::Eof)) => break,
            Err(_) => break, // best-effort: keep what we have
            _ => {}
        }
    }
    // Finalize an unterminated server (malformed input).
    if let Some(acc) = server.take() {
        push_jaxrs_server_node(rel_path, acc, &mut nodes);
    }

    IntegrationXmlOutput {
        nodes,
        edges: Vec::new(),
    }
}

fn push_jaxrs_server_node(rel_path: &str, acc: JaxrsServerAcc, nodes: &mut Vec<Node>) {
    nodes.push(Node {
        id: cih_core::integration_route_id(rel_path, &format!("jaxrs-server-{}", acc.address)),
        kind: NodeKind::IntegrationRoute,
        name: acc.address.clone(),
        qualified_name: acc.beans.first().cloned(),
        file: rel_path.to_string(),
        range: Range::default(),
        props: Some(serde_json::json!({
            "source": "cxf_jaxrs_server",
            "address": acc.address,
            "server_id": acc.server_id,
            "bean_id": acc.beans.first().cloned(),
            "beans": acc.beans,
            "bean_classes": acc.bean_classes,
        })),
    });
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
