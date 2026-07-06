//! CXF/JAX-RS route base-path stitching (Java framework logic).
//!
//! JAX-RS endpoints on OSGi/CXF projects declare their base address in Spring/Blueprint
//! XML rather than in Java annotations. `integration_xml` already parses those into
//! `cxf_jaxrs_server` / `osgi_servlet` `IntegrationRoute` nodes; this module wires them
//! onto the Java `Route` nodes, prepending `servlet_prefix + <jaxrs:server address>` to
//! each route path. Invoked from [`super::JavaResolver::post_process`].

use std::collections::HashMap;
use std::path::Path;

use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind};

/// Rewrite Java `Route` nodes so their paths include the CXF base path.
///
/// For each `<jaxrs:server address>` node we resolve its referenced bean id to a class
/// FQCN (via the `spring_xml` / `blueprint_xml` bean nodes), then rewrite every Java
/// `Route` whose `handler` belongs to that class: `path` becomes
/// `servlet_prefix + address + method_path`, the node `id`/`name` are recomputed, and the
/// `HANDLES_ROUTE` edge targeting it is repointed. The original method path is kept in
/// `local_path`, and a non-destructive `IntegrationLink` edge records provenance.
///
/// Returns early (before any filesystem scan) when there are no `<jaxrs:server>` targets.
pub(crate) fn stitch_route_prefixes(
    repo_root: &Path,
    nodes: &mut [Node],
    edges: &mut Vec<Edge>,
    route_base_path: Option<&str>,
) {
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
    // No CXF servers wired to a known class ⇒ nothing to do (and skip the fs scan below).
    if targets.is_empty() {
        return;
    }

    // Only now (there is CXF to stitch) resolve the outermost servlet base path.
    let (servlet_prefix, servlet_source) = match resolve_servlet_prefix(repo_root, nodes, route_base_path)
    {
        Some((p, s)) => (p, s),
        None => (String::new(), "none"),
    };

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

struct Target {
    server_id: NodeId,
    address: String,
    fqcn: String,
}

/// Best-effort resolution of the CXF servlet base path (e.g. `/rest`) — the outermost URL
/// layer above a `<jaxrs:server address>`. Returns the prefix and its source label, or
/// `None` when no source declares one. Priority, highest first:
///
/// 1. `config_override` — `cxf_base_path` from `cih.toml` / `--cxf-base-path`
/// 2. OSGi HTTP-whiteboard `osgi.http.whiteboard.servlet.pattern` (from `nodes`)
/// 3. `web.xml` `CXFServlet` `<url-pattern>`
/// 4. Spring Boot `cxf.path` property (`application.properties` / `.yml`)
pub(crate) fn resolve_servlet_prefix(
    repo_root: &Path,
    nodes: &[Node],
    config_override: Option<&str>,
) -> Option<(String, &'static str)> {
    if let Some(p) = config_override.map(str::trim).filter(|s| !s.is_empty()) {
        return Some((normalize_prefix(p), "config"));
    }
    if let Some(pattern) = nodes.iter().find_map(|n| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{NodeKind, Range};

    fn prop<'a>(node: &'a Node, key: &str) -> Option<&'a str> {
        node.props.as_ref()?.get(key)?.as_str()
    }

    fn integration_route(name: &str, source: &str, extra: serde_json::Value) -> Node {
        let mut props = serde_json::json!({ "source": source });
        if let (Some(obj), Some(ex)) = (props.as_object_mut(), extra.as_object()) {
            for (k, v) in ex {
                obj.insert(k.clone(), v.clone());
            }
        }
        Node {
            id: NodeId::new(format!("IntegrationRoute:{source}:{name}")),
            kind: NodeKind::IntegrationRoute,
            name: name.to_string(),
            qualified_name: extra.get("class").and_then(|v| v.as_str()).map(String::from),
            file: "beans.xml".to_string(),
            range: Range::default(),
            props: Some(props),
        }
    }

    fn route_node(method: &str, path: &str, handler: &str) -> Node {
        Node {
            id: NodeId::new(format!("Route:{method} {path}")),
            kind: NodeKind::Route,
            name: format!("{method} {path}"),
            qualified_name: Some(format!("{method} {path}")),
            file: "com/acme/Endpoint.java".to_string(),
            range: Range::default(),
            props: Some(serde_json::json!({
                "httpMethod": method,
                "path": path,
                "handler": handler,
            })),
        }
    }

    fn handles_route_edge(handler: &str, method: &str, path: &str) -> Edge {
        Edge {
            src: NodeId::new(format!("Method:{handler}")),
            dst: NodeId::new(format!("Route:{method} {path}")),
            kind: EdgeKind::HandlesRoute,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        }
    }

    /// A `<jaxrs:server address>` + its referenced bean, mirroring the parsed XML nodes.
    fn server_and_bean(address: &str, bean_id: &str, class: &str) -> Vec<Node> {
        vec![
            integration_route(
                address,
                "cxf_jaxrs_server",
                serde_json::json!({ "address": address, "bean_id": bean_id, "beans": [bean_id] }),
            ),
            integration_route(bean_id, "spring_xml", serde_json::json!({ "class": class })),
        ]
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("cih-cxf-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn servlet_prefix_config_override_wins() {
        let dir = temp_dir("cfg");
        let out = resolve_servlet_prefix(&dir, &[], Some("/rest"));
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(out, Some(("rest".to_string(), "config")));
    }

    #[test]
    fn servlet_prefix_from_web_xml() {
        let dir = temp_dir("web");
        let web = r#"<web-app>
            <servlet>
                <servlet-name>cxf</servlet-name>
                <servlet-class>org.apache.cxf.transport.servlet.CXFServlet</servlet-class>
            </servlet>
            <servlet-mapping>
                <servlet-name>cxf</servlet-name>
                <url-pattern>/services/*</url-pattern>
            </servlet-mapping>
        </web-app>"#;
        std::fs::create_dir_all(dir.join("WEB-INF")).unwrap();
        std::fs::write(dir.join("WEB-INF/web.xml"), web).unwrap();
        let out = resolve_servlet_prefix(&dir, &[], None);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(out, Some(("services".to_string(), "web_xml")));
    }

    #[test]
    fn stitch_full_prefix_rewrites_route() {
        let dir = temp_dir("stitch");
        // servlet prefix comes from an osgi_servlet node (no filesystem needed).
        let mut nodes = server_and_bean("/v1/services", "restServiceEndPointImpl", " com.acme.RestServiceEndPointImpl");
        nodes.push(integration_route(
            "/rest/*",
            "osgi_servlet",
            serde_json::json!({ "servlet_pattern": "/rest/*" }),
        ));
        let handler = "com.acme.RestServiceEndPointImpl#onOffVoice/1";
        nodes.push(route_node("POST", "/sound-box/on-off-voice", handler));
        let mut edges = vec![handles_route_edge(handler, "POST", "/sound-box/on-off-voice")];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        let full = "/rest/v1/services/sound-box/on-off-voice";
        assert_eq!(prop(route, "path"), Some(full));
        assert_eq!(route.id.as_str(), &format!("Route:POST {full}"));
        assert_eq!(prop(route, "local_path"), Some("/sound-box/on-off-voice"));
        assert_eq!(prop(route, "servlet_prefix_source"), Some("osgi_whiteboard"));

        let hr = edges
            .iter()
            .find(|e| e.kind == EdgeKind::HandlesRoute)
            .unwrap();
        assert_eq!(hr.dst.as_str(), &format!("Route:POST {full}"));

        let link = edges
            .iter()
            .find(|e| e.kind == EdgeKind::IntegrationLink && e.reason == "cxf-jaxrs-prefix")
            .expect("provenance IntegrationLink expected");
        assert_eq!(link.dst.as_str(), &format!("Route:POST {full}"));
    }

    #[test]
    fn stitch_without_servlet_layer_uses_address_only() {
        let dir = temp_dir("addr");
        let mut nodes =
            server_and_bean("/v1/services", "impl", "com.acme.RestServiceEndPointImpl");
        let handler = "com.acme.RestServiceEndPointImpl#onOffVoice/1";
        nodes.push(route_node("POST", "/sound-box/on-off-voice", handler));
        let mut edges = vec![handles_route_edge(handler, "POST", "/sound-box/on-off-voice")];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(prop(route, "path"), Some("/v1/services/sound-box/on-off-voice"));
        assert_eq!(prop(route, "servlet_prefix_source"), Some("none"));
    }

    #[test]
    fn stitch_no_matching_route_is_noop() {
        let dir = temp_dir("nomatch");
        let mut nodes = server_and_bean("/v1/services", "impl", "com.acme.RestServiceEndPointImpl");
        // A route on an unrelated class — must not be rewritten.
        nodes.push(route_node("GET", "/other", "com.acme.OtherController#get/0"));
        let mut edges = vec![handles_route_edge("com.acme.OtherController#get/0", "GET", "/other")];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, Some("/rest"));
        std::fs::remove_dir_all(&dir).ok();

        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(prop(route, "path"), Some("/other"));
        assert!(
            !edges
                .iter()
                .any(|e| e.kind == EdgeKind::IntegrationLink && e.reason == "cxf-jaxrs-prefix"),
            "no provenance edge should be emitted when nothing matched"
        );
    }
}
