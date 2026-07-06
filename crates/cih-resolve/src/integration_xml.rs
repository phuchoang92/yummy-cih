//! Phase 1b — best-effort integration XML extraction.
//!
//! Reads Camel, Blueprint, Spring XML and CXF config files and emits
//! `IntegrationRoute` / `MessageDestination` nodes plus wiring edges. This is a
//! deliberately lightweight text scanner — we do not pull in an XML parser
//! dependency. Malformed input simply yields fewer facts; it never panics.

use std::collections::HashMap;
use std::path::Path;

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
        // A CXF Spring file may declare <jaxrs:server> beside (or instead of) <bean>
        // definitions, so route the "cxf" kind through the Spring extractor too.
        "spring" | "cxf" => extract_spring_beans_xml(rel_path, content),
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

    nodes.extend(extract_cxf_jaxrs(rel_path, content));
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

    nodes.extend(extract_cxf_jaxrs(rel_path, content));
    IntegrationXmlOutput { nodes, edges }
}

/// Parse CXF JAX-RS declarations shared by the Spring and Blueprint extractors.
///
/// Emits one `IntegrationRoute` node per `<jaxrs:server address="...">`
/// (`source = "cxf_jaxrs_server"`, carrying the referenced bean ids) and one per
/// OSGi HTTP-whiteboard servlet pattern (`source = "osgi_servlet"`). These are wired
/// onto Java `Route` nodes later by [`resolve_jaxrs_xml_prefixes`].
fn extract_cxf_jaxrs(rel_path: &str, content: &str) -> Vec<Node> {
    let mut nodes = Vec::new();
    let bytes = content.as_bytes();
    let mut i = 0;
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

            if tag_name == "jaxrs:server" {
                if let Some(address) = extract_xml_attr(&content[tag_start..], "address") {
                    // Capture the element body to collect its <ref bean/component-id>s.
                    let block_end = content[tag_start..]
                        .find("</jaxrs:server>")
                        .map(|e| tag_start + e)
                        .unwrap_or(content.len());
                    let beans = collect_ref_beans(&content[tag_start..block_end]);
                    let server_id = extract_xml_attr(&content[tag_start..], "id");
                    let node_id =
                        cih_core::integration_route_id(rel_path, &format!("jaxrs-server-{address}"));
                    nodes.push(Node {
                        id: node_id,
                        kind: NodeKind::IntegrationRoute,
                        name: address.clone(),
                        qualified_name: beans.first().cloned(),
                        file: rel_path.to_string(),
                        range: Range::default(),
                        props: Some(serde_json::json!({
                            "source": "cxf_jaxrs_server",
                            "address": address,
                            "server_id": server_id,
                            "bean_id": beans.first().cloned(),
                            "beans": beans,
                        })),
                    });
                }
            } else if tag_name == "entry"
                && extract_xml_attr(&content[tag_start..], "key").as_deref()
                    == Some("osgi.http.whiteboard.servlet.pattern")
            {
                if let Some(pattern) = extract_xml_attr(&content[tag_start..], "value") {
                    let node_id = cih_core::integration_route_id(
                        rel_path,
                        &format!("osgi-servlet-{pattern}"),
                    );
                    nodes.push(Node {
                        id: node_id,
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
        i += 1;
    }
    nodes
}

/// Collect bean references inside a `<jaxrs:serviceBeans>` block. Handles both the
/// Spring form `<ref bean="..."/>` and the Blueprint form `<ref component-id="..."/>`,
/// while skipping unrelated `<reference>` elements.
fn collect_ref_beans(block: &str) -> Vec<String> {
    let bytes = block.as_bytes();
    let mut beans = Vec::new();
    let mut search = 0;
    while let Some(rel) = block[search..].find("<ref") {
        let pos = search + rel;
        let after = pos + 4;
        search = after;
        // Require `<ref` to be its own tag, not the start of `<reference`.
        match bytes.get(after) {
            Some(b) if b.is_ascii_whitespace() || *b == b'/' || *b == b'>' => {}
            _ => continue,
        }
        if let Some(bean) = extract_xml_attr(&block[pos..], "bean")
            .or_else(|| extract_xml_attr(&block[pos..], "component-id"))
        {
            beans.push(bean);
        }
    }
    beans
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

// ── CXF JAX-RS base-path stitching ──────────────────────────────────────────

/// Best-effort resolution of the CXF servlet base path (e.g. `/rest`) — the outermost
/// URL layer that sits above a `<jaxrs:server address>`. Returns the prefix and the
/// source label, or `None` when no source declares one (in which case only the jaxrs
/// address + method path are used). Priority, highest first:
///
/// 1. `config_override` — `cxf_base_path` from `cih.toml`
/// 2. OSGi HTTP-whiteboard `osgi.http.whiteboard.servlet.pattern` (from `xml_nodes`)
/// 3. `web.xml` `CXFServlet` `<url-pattern>`
/// 4. Spring Boot `cxf.path` property (`application.properties` / `.yml`)
pub fn resolve_cxf_servlet_prefix(
    repo_root: &Path,
    xml_nodes: &[Node],
    config_override: Option<&str>,
) -> Option<(String, &'static str)> {
    if let Some(p) = config_override.map(str::trim).filter(|s| !s.is_empty()) {
        return Some((normalize_prefix(p), "config"));
    }
    if let Some(pattern) = xml_nodes.iter().find_map(|n| {
        let props = n.props.as_ref()?;
        if props.get("source").and_then(|s| s.as_str()) == Some("osgi_servlet") {
            props
                .get("servlet_pattern")
                .and_then(|v| v.as_str())
                .map(String::from)
        } else {
            None
        }
    }) {
        return Some((normalize_prefix(&pattern), "osgi_whiteboard"));
    }
    if let Some(p) = scan_web_xml_cxf_prefix(repo_root) {
        return Some((normalize_prefix(&p), "web_xml"));
    }
    if let Some(p) = scan_spring_boot_cxf_path(repo_root) {
        return Some((normalize_prefix(&p), "spring_boot"));
    }
    None
}

/// Stitch XML-derived base-path prefixes onto Java `Route` nodes.
///
/// For each `<jaxrs:server address>` node we resolve its referenced bean id to a class
/// FQCN (via the `spring_xml` / `blueprint_xml` bean nodes), then rewrite every Java
/// `Route` whose `handler` belongs to that class: `path` becomes
/// `servlet_prefix + address + method_path`, the node `id`/`name` are recomputed, and
/// the `HANDLES_ROUTE` edge targeting it is repointed. The original method path is kept
/// in `local_path`, and a non-destructive `IntegrationLink` edge records provenance.
pub fn resolve_jaxrs_xml_prefixes(
    nodes: &mut [Node],
    edges: &mut Vec<Edge>,
    servlet: Option<(&str, &str)>,
) {
    let (servlet_prefix, servlet_source) = match servlet {
        Some((p, s)) => (normalize_prefix(p), s),
        None => (String::new(), "none"),
    };

    // bean id → class FQCN (repo-wide, so a server and its bean may live in different files).
    let mut bean_to_fqcn: HashMap<String, String> = HashMap::new();
    for n in nodes.iter() {
        if n.kind != NodeKind::IntegrationRoute {
            continue;
        }
        let Some(props) = n.props.as_ref() else { continue };
        let source = props.get("source").and_then(|s| s.as_str()).unwrap_or("");
        if source == "spring_xml" || source == "blueprint_xml" {
            if let Some(fqcn) = n.qualified_name.as_ref() {
                bean_to_fqcn.insert(n.name.clone(), fqcn.trim().to_string());
            }
        }
    }

    // (server node id, address, class FQCN) to apply.
    struct Target {
        server_id: NodeId,
        address: String,
        fqcn: String,
    }
    let mut targets: Vec<Target> = Vec::new();
    for n in nodes.iter() {
        if n.kind != NodeKind::IntegrationRoute {
            continue;
        }
        let Some(props) = n.props.as_ref() else { continue };
        if props.get("source").and_then(|s| s.as_str()) != Some("cxf_jaxrs_server") {
            continue;
        }
        let Some(address) = props.get("address").and_then(|s| s.as_str()) else {
            continue;
        };
        let bean_ids = props
            .get("beans")
            .and_then(|b| b.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for bean_id in bean_ids {
            if let Some(fqcn) = bean_to_fqcn.get(&bean_id) {
                targets.push(Target {
                    server_id: n.id.clone(),
                    address: address.to_string(),
                    fqcn: fqcn.clone(),
                });
            }
        }
    }
    if targets.is_empty() {
        return;
    }

    let mut id_remap: HashMap<NodeId, NodeId> = HashMap::new();
    let mut new_edges: Vec<Edge> = Vec::new();

    for n in nodes.iter_mut() {
        if n.kind != NodeKind::Route {
            continue;
        }
        let Some(props) = n.props.as_mut() else { continue };
        let handler = props
            .get("handler")
            .and_then(|h| h.as_str())
            .unwrap_or("")
            .to_string();
        if handler.is_empty() {
            continue;
        }
        let Some(target) = targets
            .iter()
            .find(|t| handler == t.fqcn || handler.starts_with(&format!("{}#", t.fqcn)))
        else {
            continue;
        };

        let method = props
            .get("httpMethod")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let local_path = props
            .get("path")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string();
        let new_path = join_url(&[&servlet_prefix, &target.address, &local_path]);
        if new_path == local_path {
            continue;
        }

        props["path"] = serde_json::Value::String(new_path.clone());
        props["local_path"] = serde_json::Value::String(local_path);
        props["servlet_prefix_source"] = serde_json::Value::String(servlet_source.to_string());

        let new_name = format!("{method} {new_path}");
        let new_id = NodeId::new(format!("Route:{new_name}"));
        let old_id = std::mem::replace(&mut n.id, new_id.clone());
        n.name = new_name.clone();
        n.qualified_name = Some(new_name);

        new_edges.push(Edge {
            src: target.server_id.clone(),
            dst: new_id.clone(),
            kind: EdgeKind::IntegrationLink,
            confidence: 0.9,
            reason: "cxf-jaxrs-prefix".to_string(),
            props: Some(serde_json::json!({
                "source": "cxf_jaxrs_server",
                "prefix": join_url(&[&servlet_prefix, &target.address]),
            })),
        });
        id_remap.insert(old_id, new_id);
    }

    if id_remap.is_empty() {
        return;
    }
    // Repoint edges (notably HANDLES_ROUTE) that targeted the rewritten Route ids.
    for e in edges.iter_mut() {
        if let Some(new_id) = id_remap.get(&e.dst) {
            e.dst = new_id.clone();
        }
    }
    edges.extend(new_edges);
}

/// Normalize a servlet/base-path prefix: strip a trailing `/*` (or `*`) wildcard and any
/// trailing slash. The leading slash is added by [`join_url`], so `"/rest/*"`, `"rest"`,
/// and `"/rest/"` all normalize to `"rest"`-equivalent segments.
fn normalize_prefix(raw: &str) -> String {
    let t = raw.trim();
    let t = t
        .strip_suffix("/*")
        .or_else(|| t.strip_suffix('*'))
        .unwrap_or(t);
    t.trim_matches('/').to_string()
}

/// Join URL path segments with single slashes and a single leading slash, dropping empty
/// pieces (so an absent servlet prefix collapses cleanly).
fn join_url(segments: &[&str]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in segments {
        for piece in seg.split('/') {
            if !piece.is_empty() {
                parts.push(piece);
            }
        }
    }
    format!("/{}", parts.join("/"))
}

/// Walk `repo_root` and return the contents of every file whose name matches `name_match`.
/// Best-effort: unreadable files and walk errors are skipped.
fn walk_file_contents(repo_root: &Path, name_match: impl Fn(&str) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(repo_root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .build();
    for result in walker {
        let Ok(entry) = result else { continue };
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if name_match(name) {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                out.push(content);
            }
        }
    }
    out
}

/// Find the `<url-pattern>` mapped to a `CXFServlet` in any `web.xml`.
fn scan_web_xml_cxf_prefix(repo_root: &Path) -> Option<String> {
    for content in walk_file_contents(repo_root, |n| n.eq_ignore_ascii_case("web.xml")) {
        if !content.contains("CXFServlet") {
            continue;
        }
        if let Some(p) = web_xml_cxf_url_pattern(&content) {
            return Some(p);
        }
    }
    None
}

fn web_xml_cxf_url_pattern(content: &str) -> Option<String> {
    let servlet_name = element_blocks(content, "servlet").into_iter().find_map(|blk| {
        let class = inner_text(blk, "servlet-class")?;
        if class.contains("CXFServlet") {
            inner_text(blk, "servlet-name")
        } else {
            None
        }
    })?;
    for blk in element_blocks(content, "servlet-mapping") {
        if inner_text(blk, "servlet-name").as_deref().map(str::trim) == Some(servlet_name.trim()) {
            if let Some(p) = inner_text(blk, "url-pattern") {
                return Some(p);
            }
        }
    }
    None
}

/// Inner text of each `<tag>…</tag>` element (no attribute handling; best-effort).
fn element_blocks<'a>(content: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(s) = content[search..].find(&open) {
        let start = search + s + open.len();
        let Some(e) = content[start..].find(&close) else {
            break;
        };
        out.push(&content[start..start + e]);
        search = start + e + close.len();
    }
    out
}

fn inner_text(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let s = block.find(&open)? + open.len();
    let e = block[s..].find(&close)?;
    Some(block[s..s + e].trim().to_string())
}

/// Find a Spring Boot `cxf.path` property in `application.properties` / `.yml`.
fn scan_spring_boot_cxf_path(repo_root: &Path) -> Option<String> {
    for content in walk_file_contents(repo_root, |n| {
        n.eq_ignore_ascii_case("application.properties")
            || (n.starts_with("application-") && n.ends_with(".properties"))
    }) {
        for line in content.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("cxf.path") {
                if let Some(val) = rest.trim_start().strip_prefix('=') {
                    let val = unquote(val.trim());
                    if !val.is_empty() {
                        return Some(val);
                    }
                }
            }
        }
    }
    for content in walk_file_contents(repo_root, |n| {
        n.eq_ignore_ascii_case("application.yml") || n.eq_ignore_ascii_case("application.yaml")
    }) {
        if let Some(p) = yaml_cxf_path(&content) {
            return Some(p);
        }
    }
    None
}

fn yaml_cxf_path(content: &str) -> Option<String> {
    let mut in_cxf = false;
    for line in content.lines() {
        if line.trim().starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let t = line.trim();
        if indent == 0 {
            if let Some(rest) = t.strip_prefix("cxf.path:") {
                let v = unquote(rest.trim());
                if !v.is_empty() {
                    return Some(v);
                }
            }
            in_cxf = t == "cxf:";
        } else if in_cxf {
            if let Some(rest) = t.strip_prefix("path:") {
                let v = unquote(rest.trim());
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

fn unquote(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '\'').to_string()
}

