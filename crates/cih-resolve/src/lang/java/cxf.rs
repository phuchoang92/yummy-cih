//! CXF/JAX-RS route base-path stitching (Java framework logic).
//!
//! JAX-RS endpoints on OSGi/CXF projects declare their base address in Spring/Blueprint
//! XML rather than in Java annotations. `integration_xml` already parses those into
//! `cxf_jaxrs_server` / `osgi_servlet` `IntegrationRoute` nodes; this module wires them
//! onto the Java `Route` nodes, prepending `servlet_prefix + <jaxrs:server address>` to
//! each route path. Invoked from [`super::JavaResolver::post_process`].

use rustc_hash::FxHashMap;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind};

/// Rewrite Java `Route` nodes so their paths include the CXF base path.
///
/// For each `<jaxrs:server address>` node we resolve its referenced bean id to a class
/// FQCN (via the `spring_xml` / `blueprint_xml` bean nodes), then rewrite every Java
/// `Route` whose `handler` belongs to that class — or to an interface the class
/// (transitively) implements, since JAX-RS annotations often live on the interface:
/// `path` becomes `servlet_prefix + address + method_path`, the node `id`/`name` are
/// recomputed, and edges targeting it (notably `HANDLES_ROUTE`) are repointed. When a
/// route matches SEVERAL servers with distinct addresses (secured `/v1` + non-secured
/// `/ns/v1` impls of one interface), the first rewrites in place and each further
/// address becomes a cloned Route node with duplicated incoming edges. The original
/// method path is kept in `local_path`, and a non-destructive `IntegrationLink` edge
/// records provenance per server.
///
/// Returns early (before any filesystem scan) when there are no `<jaxrs:server>` targets.
/// Resolve a bean `class` string to a class FQCN: a fully-qualified name is kept as-is; a bare
/// simple name resolves to a workspace-unique FQCN (else it is left as-is, unresolved).
fn resolve_class_fqcn(raw: &str, simple_to_fqcns: &HashMap<&str, Vec<&str>>) -> String {
    if raw.contains('.') {
        return raw.to_string();
    }
    match simple_to_fqcns.get(raw) {
        Some(fqcns) if fqcns.len() == 1 => fqcns[0].to_string(),
        _ => raw.to_string(),
    }
}

pub(crate) fn stitch_route_prefixes(
    repo_root: &Path,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    route_base_path: Option<&str>,
) {
    // Nothing to stitch unless a <jaxrs:server> exists — bail before building maps or scanning fs.
    let has_cxf = nodes.iter().any(|n| {
        n.kind == NodeKind::IntegrationRoute
            && n.props
                .as_ref()
                .and_then(|p| p.get("source"))
                .and_then(|s| s.as_str())
                == Some("cxf_jaxrs_server")
    });
    if !has_cxf {
        return;
    }

    // The ResolveIndex isn't in scope in `post_process`, so derive the equivalent lookups from the
    // assembled graph: FQCN → Class node (existence + edge target) and simple name → FQCNs (for
    // the workspace-unique-name fallback, mirroring ResolveIndex).
    let mut class_node_by_fqcn: HashMap<&str, &NodeId> = HashMap::new();
    let mut simple_to_fqcns: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut kind_by_fqcn: HashMap<&str, NodeKind> = HashMap::new();
    let mut fqcn_by_node_id: HashMap<&NodeId, &str> = HashMap::new();
    for n in nodes.iter() {
        if matches!(
            n.kind,
            NodeKind::Class | NodeKind::Interface | NodeKind::Enum | NodeKind::Record
        ) {
            if let Some(fqcn) = n.qualified_name.as_deref() {
                class_node_by_fqcn.insert(fqcn, &n.id);
                kind_by_fqcn.insert(fqcn, n.kind);
                fqcn_by_node_id.insert(&n.id, fqcn);
                simple_to_fqcns
                    .entry(crate::di_xml::simple_name(fqcn))
                    .or_default()
                    .push(fqcn);
            }
        }
    }

    // Type → supertypes adjacency from heritage edges (src = subtype, dst =
    // supertype). Feeds the interface-fallback: JAX-RS annotations often live
    // on the interface in a separate `-api` bundle while the jaxrs:server bean
    // is the impl class, so routes carry the interface FQCN as handler.
    let mut supers: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in edges.iter() {
        if !matches!(e.kind, EdgeKind::Implements | EdgeKind::Extends) {
            continue;
        }
        if let (Some(sub), Some(sup)) = (fqcn_by_node_id.get(&e.src), fqcn_by_node_id.get(&e.dst))
        {
            supers.entry(sub).or_default().push(sup);
        }
    }

    // bean id → (bean node id, class FQCN), repo-wide. A simple-name `class` is resolved to a
    // workspace-unique FQCN; a fully-qualified `class` is kept as-is.
    let mut bean_index: HashMap<String, (NodeId, String)> = HashMap::new();
    for n in nodes.iter() {
        if n.kind != NodeKind::IntegrationRoute {
            continue;
        }
        let Some(props) = n.props.as_ref() else {
            continue;
        };
        let source = props.get("source").and_then(|s| s.as_str()).unwrap_or("");
        if source != "spring_xml" && source != "blueprint_xml" {
            continue;
        }
        let Some(class) = props.get("class").and_then(|c| c.as_str()) else {
            continue;
        };
        let fqcn = resolve_class_fqcn(class.trim(), &simple_to_fqcns);
        bean_index.insert(n.name.clone(), (n.id.clone(), fqcn));
    }

    // (server node id, address, class FQCN) to apply, plus explicit bean → Class registration edges.
    let mut targets: Vec<Target> = Vec::new();
    let mut new_edges: Vec<Edge> = Vec::new();
    let mut bean_class_seen: HashSet<(NodeId, NodeId)> = HashSet::new();
    for n in nodes.iter() {
        if n.kind != NodeKind::IntegrationRoute {
            continue;
        }
        let Some(props) = n.props.as_ref() else {
            continue;
        };
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
            let Some((bean_node_id, fqcn)) = bean_index.get(&bean_id) else {
                continue;
            };
            targets.push(Target {
                server_id: n.id.clone(),
                server_file: n.file.clone(),
                address: address.to_string(),
                fqcn: fqcn.clone(),
                interfaces: HashSet::new(),
            });
            // Make the bean → impl-class registration an explicit, queryable edge (previously the
            // linkage was only an implicit FQCN prefix-match on Route.handler).
            if let Some(class_id) = class_node_by_fqcn.get(fqcn.as_str()) {
                if bean_class_seen.insert((bean_node_id.clone(), (*class_id).clone())) {
                    new_edges.push(Edge {
                        src: bean_node_id.clone(),
                        dst: (*class_id).clone(),
                        kind: EdgeKind::IntegrationLink,
                        confidence: 0.9,
                        reason: "cxf-bean-class".to_string(),
                        props: None,
                    });
                }
            }
        }

        // Anonymous inline serviceBeans (`<jaxrs:serviceBeans><bean class=…/></...>`) have no id,
        // so the class travels on the server node; the registration edge originates there.
        let bean_classes = props
            .get("bean_classes")
            .and_then(|b| b.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for raw in bean_classes {
            let fqcn = resolve_class_fqcn(raw.trim(), &simple_to_fqcns);
            targets.push(Target {
                server_id: n.id.clone(),
                server_file: n.file.clone(),
                address: address.to_string(),
                fqcn: fqcn.clone(),
                interfaces: HashSet::new(),
            });
            if let Some(class_id) = class_node_by_fqcn.get(fqcn.as_str()) {
                if bean_class_seen.insert((n.id.clone(), (*class_id).clone())) {
                    new_edges.push(Edge {
                        src: n.id.clone(),
                        dst: (*class_id).clone(),
                        kind: EdgeKind::IntegrationLink,
                        confidence: 0.9,
                        reason: "cxf-bean-class".to_string(),
                        props: None,
                    });
                }
            }
        }
    }
    // No CXF servers wired to a known bean ⇒ nothing to do (and skip the fs scan below).
    if targets.is_empty() {
        return;
    }

    // Attach each target's interface closure (memoized per FQCN) for the
    // handler fallback match.
    {
        let mut closure_memo: HashMap<String, HashSet<String>> = HashMap::new();
        for t in &mut targets {
            t.interfaces = closure_memo
                .entry(t.fqcn.clone())
                .or_insert_with(|| supertype_interfaces(&t.fqcn, &supers, &kind_by_fqcn))
                .clone();
        }
    }

    // Servlet base paths are per-bundle on OSGi platforms (each bundle declares
    // its own whiteboard pattern), so resolve them per server file, memoized.
    let resolver = ServletPrefixResolver::build(nodes, route_base_path);
    let mut prefix_memo: FxHashMap<String, (String, &'static str)> = FxHashMap::default();

    let mut id_remap: FxHashMap<NodeId, NodeId> = FxHashMap::default();
    // Additional matches beyond the first clone the route: a secured and a
    // non-secured jaxrs:server exposing the same annotated interface are two
    // real URLs (`/v1/…` and `/ns/v1/…`), each needing its own Route node.
    let mut clones: Vec<Node> = Vec::new();
    let mut clone_map: Vec<(NodeId, NodeId)> = Vec::new();
    let mut route_ids_taken: HashSet<NodeId> = nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Route)
        .map(|n| n.id.clone())
        .collect();

    for n in nodes.iter_mut() {
        if n.kind != NodeKind::Route {
            continue;
        }
        let Some(props) = n.props.as_ref() else {
            continue;
        };
        let handler = props
            .get("handler")
            .and_then(|h| h.as_str())
            .unwrap_or("")
            .to_string();
        if handler.is_empty() {
            continue;
        }
        // Exact impl-class matches keep absolute priority; the interface
        // fallback (handler = annotated interface, bean = impl) only applies
        // when no target names the handler's class directly.
        let handler_class = handler.split('#').next().unwrap_or(handler.as_str());
        let mut matched: Vec<&Target> =
            targets.iter().filter(|t| t.fqcn == handler_class).collect();
        if matched.is_empty() {
            matched = targets
                .iter()
                .filter(|t| t.interfaces.contains(handler_class))
                .collect();
        }
        if matched.is_empty() {
            continue;
        }

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

        // Only distinct resulting paths produce routes: identical addresses
        // across matching servers (or a no-op prefix) collapse to one.
        let mut seen_paths: HashSet<String> = HashSet::new();
        seen_paths.insert(local_path.clone());
        // Set once the route is rewritten in place — the ORIGINAL id, which
        // clones copy their incoming edges from.
        let mut original_id: Option<NodeId> = None;

        for target in matched {
            let (servlet_prefix, servlet_source) = prefix_memo
                .entry(target.server_file.clone())
                .or_insert_with(|| {
                    resolver
                        .prefix_for(repo_root, &target.server_file)
                        .unwrap_or((String::new(), "none"))
                })
                .clone();

            let new_path = join_url(&[&servlet_prefix, &target.address, &local_path]);
            if !seen_paths.insert(new_path.clone()) {
                continue;
            }
            let new_name = format!("{method} {new_path}");
            let new_id = NodeId::new(format!("Route:{new_name}"));
            let provenance = Edge {
                src: target.server_id.clone(),
                dst: new_id.clone(),
                kind: EdgeKind::IntegrationLink,
                confidence: 0.9,
                reason: "cxf-jaxrs-prefix".to_string(),
                props: Some(serde_json::json!({
                    "source": "cxf_jaxrs_server",
                    "prefix": join_url(&[&servlet_prefix, &target.address]),
                })),
            };

            match &original_id {
                // First changed path: rewrite the node in place, as always.
                None => {
                    if let Some(props) = n.props.as_mut() {
                        props["path"] = serde_json::Value::String(new_path.clone());
                        props["local_path"] = serde_json::Value::String(local_path.clone());
                        props["servlet_prefix_source"] =
                            serde_json::Value::String(servlet_source.to_string());
                    }
                    let old_id = std::mem::replace(&mut n.id, new_id.clone());
                    n.name = new_name.clone();
                    n.qualified_name = Some(new_name);
                    route_ids_taken.insert(new_id.clone());
                    new_edges.push(provenance);
                    id_remap.insert(old_id.clone(), new_id);
                    original_id = Some(old_id);
                }
                // Every further changed path: clone the (already rewritten)
                // node with its own path props. The handler stays — the same
                // annotated method genuinely serves both variants.
                Some(old_id) => {
                    if !route_ids_taken.insert(new_id.clone()) {
                        continue; // a pre-existing route already owns this id
                    }
                    let mut clone = n.clone();
                    clone.id = new_id.clone();
                    clone.name = new_name.clone();
                    clone.qualified_name = Some(new_name);
                    if let Some(cp) = clone.props.as_mut() {
                        cp["path"] = serde_json::Value::String(new_path.clone());
                        cp["local_path"] = serde_json::Value::String(local_path.clone());
                        cp["servlet_prefix_source"] =
                            serde_json::Value::String(servlet_source.to_string());
                    }
                    new_edges.push(provenance);
                    clone_map.push((old_id.clone(), new_id));
                    clones.push(clone);
                }
            }
        }
    }

    if id_remap.is_empty() {
        return;
    }
    // FIRST duplicate edges (notably HANDLES_ROUTE) onto the clones, keyed on
    // the routes' ORIGINAL ids — the in-place repoint below rewrites those same
    // ids, so order matters.
    if !clone_map.is_empty() {
        let mut dup_edges: Vec<Edge> = Vec::new();
        for (old_id, clone_id) in &clone_map {
            for e in edges.iter() {
                if &e.dst == old_id {
                    let mut dup = e.clone();
                    dup.dst = clone_id.clone();
                    dup_edges.push(dup);
                }
            }
        }
        edges.extend(dup_edges);
    }
    // Then repoint edges that targeted the rewritten Route ids.
    for e in edges.iter_mut() {
        if let Some(new_id) = id_remap.get(&e.dst) {
            e.dst = new_id.clone();
        }
    }
    edges.extend(new_edges);
    nodes.extend(clones);
}

struct Target {
    server_id: NodeId,
    /// Repo-relative path of the XML file declaring the `<jaxrs:server>` —
    /// the anchor for per-bundle servlet-prefix resolution.
    server_file: String,
    address: String,
    fqcn: String,
    /// Interfaces the bean class (transitively) implements — fallback match
    /// set for routes whose handler is the annotated interface, not the impl.
    interfaces: HashSet<String>,
}

/// All interfaces reachable from `fqcn` via heritage edges (`impl implements I`,
/// `I extends J`, through superclasses too). BFS with a defensive depth cap;
/// non-interface supertypes are traversed but not returned.
fn supertype_interfaces(
    fqcn: &str,
    supers: &HashMap<&str, Vec<&str>>,
    kind_by_fqcn: &HashMap<&str, NodeKind>,
) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut visited: HashSet<&str> = HashSet::new();
    let mut queue: Vec<&str> = vec![fqcn];
    for _depth in 0..64 {
        if queue.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for cur in queue.drain(..) {
            if !visited.insert(cur) {
                continue;
            }
            for sup in supers.get(cur).into_iter().flatten() {
                if kind_by_fqcn.get(sup) == Some(&NodeKind::Interface) {
                    out.insert((*sup).to_string());
                }
                next.push(*sup);
            }
        }
        queue = next;
    }
    out
}

/// Per-bundle servlet base-path resolution (the outermost URL layer above a
/// `<jaxrs:server address>`). Built once per stitch from the graph nodes.
///
/// Priority per server: config override (global) → the OSGi whiteboard pattern
/// whose declaring file shares the most leading directory components with the
/// server's file (each bundle declares its own `/rest/<name>/*`) → a lone
/// repo-wide pattern (single-bundle repos, where files may share no directory)
/// → `web.xml` / Spring Boot `cxf.path` (global, lazily scanned) → none.
struct ServletPrefixResolver {
    config: Option<String>,
    /// `(declaring file, normalized pattern)`, sorted so equal-score ties
    /// resolve to the lexicographically first file deterministically.
    osgi: Vec<(String, String)>,
    fs_fallback: std::cell::OnceCell<Option<(String, &'static str)>>,
}

impl ServletPrefixResolver {
    fn build(nodes: &[Node], config_override: Option<&str>) -> Self {
        let config = config_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(normalize_prefix);
        let mut osgi: Vec<(String, String)> = nodes
            .iter()
            .filter_map(|n| {
                let props = n.props.as_ref()?;
                if props.get("source")?.as_str()? != "osgi_servlet" {
                    return None;
                }
                let pattern = props.get("servlet_pattern")?.as_str()?;
                Some((n.file.clone(), normalize_prefix(pattern)))
            })
            .collect();
        osgi.sort();
        osgi.dedup();
        Self {
            config,
            osgi,
            fs_fallback: std::cell::OnceCell::new(),
        }
    }

    fn fs_fallback(&self, repo_root: &Path) -> Option<(String, &'static str)> {
        self.fs_fallback
            .get_or_init(|| {
                if let Some(p) = scan_web_xml_cxf_prefix(repo_root) {
                    return Some((normalize_prefix(&p), "web_xml"));
                }
                if let Some(p) = scan_spring_boot_cxf_path(repo_root) {
                    return Some((normalize_prefix(&p), "spring_boot"));
                }
                None
            })
            .clone()
    }

    fn prefix_for(&self, repo_root: &Path, server_file: &str) -> Option<(String, &'static str)> {
        if let Some(p) = &self.config {
            return Some((p.clone(), "config"));
        }
        let server_dir = dir_components(server_file);
        let mut best: Option<(usize, &(String, String))> = None;
        for entry in &self.osgi {
            let score = shared_leading(&server_dir, &dir_components(&entry.0));
            if score == 0 {
                continue;
            }
            let better = match best {
                None => true,
                Some((best_score, best_entry)) => {
                    score > best_score
                        || (score == best_score && entry.0.len() < best_entry.0.len())
                }
            };
            if better {
                best = Some((score, entry));
            }
        }
        if let Some((_, entry)) = best {
            return Some((entry.1.clone(), "osgi_whiteboard"));
        }
        // A lone pattern still applies repo-wide (single-bundle repos, or
        // synthetic graphs whose nodes carry no directory structure). Multiple
        // unrelated patterns must NOT cross-apply — skip the osgi layer.
        if self.osgi.len() == 1 {
            return Some((self.osgi[0].1.clone(), "osgi_whiteboard"));
        }
        self.fs_fallback(repo_root)
    }
}

/// Directory components of a repo-relative path (file name dropped).
fn dir_components(path: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    parts.pop();
    parts
}

fn shared_leading(a: &[&str], b: &[&str]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// Best-effort resolution of the CXF servlet base path (e.g. `/rest`) without a
/// per-bundle anchor — [`ServletPrefixResolver::prefix_for`] with no server file,
/// so only the config override / lone-pattern / filesystem layers apply. Kept as
/// the stable entry point for callers (and tests) that predate per-bundle
/// resolution. Priority, highest first:
///
/// 1. `config_override` — `cxf_base_path` from `cih.toml` / `--cxf-base-path`
/// 2. a lone OSGi HTTP-whiteboard `osgi.http.whiteboard.servlet.pattern` (from `nodes`)
/// 3. `web.xml` `CXFServlet` `<url-pattern>`
/// 4. Spring Boot `cxf.path` property (`application.properties` / `.yml`)
#[cfg(test)]
fn resolve_servlet_prefix(
    repo_root: &Path,
    nodes: &[Node],
    config_override: Option<&str>,
) -> Option<(String, &'static str)> {
    ServletPrefixResolver::build(nodes, config_override).prefix_for(repo_root, "")
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
    let servlet_name = element_blocks(content, "servlet")
        .into_iter()
        .find_map(|blk| {
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
#[path = "cxf_tests.rs"]
mod tests;