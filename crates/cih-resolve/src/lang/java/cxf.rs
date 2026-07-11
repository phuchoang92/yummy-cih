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
            qualified_name: extra
                .get("class")
                .and_then(|v| v.as_str())
                .map(String::from),
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

    /// Re-home a synthetic node to a specific repo-relative file (the default
    /// helpers hardcode `beans.xml`, which puts everything in one "bundle").
    fn at_file(mut node: Node, file: &str) -> Node {
        node.file = file.to_string();
        node
    }

    fn interface_node(fqcn: &str) -> Node {
        Node {
            id: NodeId::new(format!("Interface:{fqcn}")),
            kind: NodeKind::Interface,
            name: crate::di_xml::simple_name(fqcn).to_string(),
            qualified_name: Some(fqcn.to_string()),
            file: "com/acme/Api.java".to_string(),
            range: Range::default(),
            props: None,
        }
    }

    /// A heritage edge as `emit_heritage_edges` produces it: subtype → supertype.
    fn heritage_edge(kind: EdgeKind, sub_id: &str, super_id: &str) -> Edge {
        Edge {
            src: NodeId::new(sub_id),
            dst: NodeId::new(super_id),
            kind,
            confidence: 1.0,
            reason: "heritage".to_string(),
            props: None,
        }
    }

    /// One OCB-style bundle: a `<jaxrs:server>` + bean in `beans_rest.xml` and a
    /// whiteboard servlet pattern in `beans_rest_web_servlets.xml`, all under
    /// `<bundle_dir>/resources/META-INF/spring/`.
    fn bundle(bundle_dir: &str, pattern: &str, address: &str, bean_id: &str, class: &str) -> Vec<Node> {
        let spring_dir = format!("{bundle_dir}/resources/META-INF/spring");
        let mut nodes: Vec<Node> = server_and_bean(address, bean_id, class)
            .into_iter()
            .map(|n| {
                let file = format!("{spring_dir}/beans_rest.xml");
                at_file(n, &file)
            })
            .collect();
        nodes.push(at_file(
            integration_route(
                pattern,
                "osgi_servlet",
                serde_json::json!({ "servlet_pattern": pattern }),
            ),
            &format!("{spring_dir}/beans_rest_web_servlets.xml"),
        ));
        nodes
    }


    #[test]
    fn stitch_interface_handler_via_impl_class() {
        // OCB shape: @Path lives on the interface in the -api bundle; the
        // jaxrs:server bean is the impl. The route's handler is the interface.
        let dir = temp_dir("iface");
        let mut nodes = server_and_bean("/v1", "restImpl", "com.acme.RestImpl");
        nodes.push(class_node("com.acme.RestImpl"));
        nodes.push(interface_node("com.acme.api.RestService"));
        let handler = "com.acme.api.RestService#op/1";
        nodes.push(route_node("GET", "/op", handler));
        let mut edges = vec![
            handles_route_edge(handler, "GET", "/op"),
            heritage_edge(
                EdgeKind::Implements,
                "Class:com.acme.RestImpl",
                "Interface:com.acme.api.RestService",
            ),
        ];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(prop(route, "path"), Some("/v1/op"));
        assert_eq!(prop(route, "local_path"), Some("/op"));
        let hr = edges
            .iter()
            .find(|e| e.kind == EdgeKind::HandlesRoute)
            .unwrap();
        assert_eq!(hr.dst.as_str(), "Route:GET /v1/op");
        assert!(edges
            .iter()
            .any(|e| e.kind == EdgeKind::IntegrationLink && e.reason == "cxf-jaxrs-prefix"));
    }

    #[test]
    fn stitch_interface_fallback_transitive_extends() {
        // Impl implements A, interface A extends B, annotations on B.
        let dir = temp_dir("iface-trans");
        let mut nodes = server_and_bean("/v1", "restImpl", "com.acme.RestImpl");
        nodes.push(class_node("com.acme.RestImpl"));
        nodes.push(interface_node("com.acme.api.A"));
        nodes.push(interface_node("com.acme.api.B"));
        let handler = "com.acme.api.B#op/0";
        nodes.push(route_node("GET", "/op", handler));
        let mut edges = vec![
            handles_route_edge(handler, "GET", "/op"),
            heritage_edge(
                EdgeKind::Implements,
                "Class:com.acme.RestImpl",
                "Interface:com.acme.api.A",
            ),
            heritage_edge(
                EdgeKind::Extends,
                "Interface:com.acme.api.A",
                "Interface:com.acme.api.B",
            ),
        ];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(prop(route, "path"), Some("/v1/op"));
    }

    #[test]
    fn stitch_exact_impl_match_beats_interface_fallback() {
        // Two servers: one names the handler's class exactly, the other only
        // reaches it via the interface set. The exact one must win.
        let dir = temp_dir("exact-wins");
        let mut nodes = server_and_bean("/direct", "implBean", "com.acme.RestImpl");
        nodes.extend(server_and_bean("/other", "otherBean", "com.acme.OtherImpl"));
        nodes.push(class_node("com.acme.RestImpl"));
        nodes.push(class_node("com.acme.OtherImpl"));
        nodes.push(interface_node("com.acme.api.RestService"));
        // OtherImpl implements the interface; the route handler is the IMPL
        // class RestImpl, so the /direct server matches exactly.
        let handler = "com.acme.RestImpl#op/0";
        nodes.push(route_node("GET", "/op", handler));
        let mut edges = vec![
            handles_route_edge(handler, "GET", "/op"),
            heritage_edge(
                EdgeKind::Implements,
                "Class:com.acme.RestImpl",
                "Interface:com.acme.api.RestService",
            ),
            heritage_edge(
                EdgeKind::Implements,
                "Class:com.acme.OtherImpl",
                "Interface:com.acme.api.RestService",
            ),
        ];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(prop(route, "path"), Some("/direct/op"));
    }

    #[test]
    fn stitch_interface_handler_without_heritage_is_noop() {
        let dir = temp_dir("iface-none");
        let mut nodes = server_and_bean("/v1", "restImpl", "com.acme.RestImpl");
        nodes.push(class_node("com.acme.RestImpl"));
        nodes.push(interface_node("com.acme.api.Unrelated"));
        let handler = "com.acme.api.Unrelated#op/0";
        nodes.push(route_node("GET", "/op", handler));
        let mut edges = vec![handles_route_edge(handler, "GET", "/op")];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(prop(route, "path"), Some("/op"));
        assert!(prop(route, "local_path").is_none());
        assert!(!edges
            .iter()
            .any(|e| e.kind == EdgeKind::IntegrationLink && e.reason == "cxf-jaxrs-prefix"));
    }


    #[test]
    fn stitch_dual_servers_clone_route_per_address() {
        // Secured /v1 and non-secured /ns/v1 servers, two impl beans, one
        // annotated interface: one handler must yield TWO routes.
        let dir = temp_dir("dual");
        let mut nodes = server_and_bean("/v1", "securedImpl", "com.acme.SecuredImpl");
        nodes.extend(server_and_bean("/ns/v1", "nonSecuredImpl", "com.acme.NonSecuredImpl"));
        nodes.push(class_node("com.acme.SecuredImpl"));
        nodes.push(class_node("com.acme.NonSecuredImpl"));
        nodes.push(interface_node("com.acme.api.RemitService"));
        let handler = "com.acme.api.RemitService#send/1";
        nodes.push(route_node("POST", "/send", handler));
        let mut edges = vec![
            handles_route_edge(handler, "POST", "/send"),
            heritage_edge(
                EdgeKind::Implements,
                "Class:com.acme.SecuredImpl",
                "Interface:com.acme.api.RemitService",
            ),
            heritage_edge(
                EdgeKind::Implements,
                "Class:com.acme.NonSecuredImpl",
                "Interface:com.acme.api.RemitService",
            ),
        ];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let routes: Vec<&Node> = nodes.iter().filter(|n| n.kind == NodeKind::Route).collect();
        assert_eq!(routes.len(), 2, "one route per server address");
        let mut paths: Vec<&str> = routes.iter().filter_map(|n| prop(n, "path")).collect();
        paths.sort();
        assert_eq!(paths, vec!["/ns/v1/send", "/v1/send"]);
        for r in &routes {
            assert_eq!(prop(r, "local_path"), Some("/send"));
            assert_eq!(prop(r, "handler"), Some(handler));
        }

        let hr: Vec<&Edge> = edges
            .iter()
            .filter(|e| e.kind == EdgeKind::HandlesRoute)
            .collect();
        assert_eq!(hr.len(), 2, "HANDLES_ROUTE duplicated onto the clone");
        assert_eq!(hr[0].src, hr[1].src, "same handler method");
        let mut hr_dsts: Vec<&str> = hr.iter().map(|e| e.dst.as_str()).collect();
        hr_dsts.sort();
        assert_eq!(hr_dsts, vec!["Route:POST /ns/v1/send", "Route:POST /v1/send"]);

        // Each route has a provenance link from its own server.
        let links: Vec<&Edge> = edges
            .iter()
            .filter(|e| e.kind == EdgeKind::IntegrationLink && e.reason == "cxf-jaxrs-prefix")
            .collect();
        assert_eq!(links.len(), 2);
        assert_ne!(links[0].src, links[1].src, "distinct server nodes");
    }

    #[test]
    fn stitch_dual_servers_same_resulting_path_dedups() {
        // Two servers with the SAME address referencing the two impls: paths
        // collide, so no clone is made.
        let dir = temp_dir("dual-same");
        let mut nodes = server_and_bean("/v1", "securedImpl", "com.acme.SecuredImpl");
        nodes.extend(server_and_bean("/v1", "nonSecuredImpl", "com.acme.NonSecuredImpl"));
        nodes.push(class_node("com.acme.SecuredImpl"));
        nodes.push(class_node("com.acme.NonSecuredImpl"));
        nodes.push(interface_node("com.acme.api.RemitService"));
        let handler = "com.acme.api.RemitService#send/1";
        nodes.push(route_node("POST", "/send", handler));
        let mut edges = vec![
            handles_route_edge(handler, "POST", "/send"),
            heritage_edge(
                EdgeKind::Implements,
                "Class:com.acme.SecuredImpl",
                "Interface:com.acme.api.RemitService",
            ),
            heritage_edge(
                EdgeKind::Implements,
                "Class:com.acme.NonSecuredImpl",
                "Interface:com.acme.api.RemitService",
            ),
        ];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let routes: Vec<&Node> = nodes.iter().filter(|n| n.kind == NodeKind::Route).collect();
        assert_eq!(routes.len(), 1);
        assert_eq!(prop(routes[0], "path"), Some("/v1/send"));
        assert_eq!(
            edges
                .iter()
                .filter(|e| e.kind == EdgeKind::HandlesRoute)
                .count(),
            1
        );
    }

    #[test]
    fn stitch_clone_skipped_when_id_already_exists() {
        // A pre-existing route already owns the would-be clone id: no duplicate node.
        let dir = temp_dir("dual-collide");
        let mut nodes = server_and_bean("/v1", "securedImpl", "com.acme.SecuredImpl");
        nodes.extend(server_and_bean("/ns/v1", "nonSecuredImpl", "com.acme.NonSecuredImpl"));
        nodes.push(class_node("com.acme.SecuredImpl"));
        nodes.push(class_node("com.acme.NonSecuredImpl"));
        nodes.push(interface_node("com.acme.api.RemitService"));
        let handler = "com.acme.api.RemitService#send/1";
        nodes.push(route_node("POST", "/send", handler));
        // Unrelated pre-existing route occupying the clone's id.
        nodes.push(route_node("POST", "/ns/v1/send", "com.acme.Other#send/1"));
        let mut edges = vec![
            handles_route_edge(handler, "POST", "/send"),
            heritage_edge(
                EdgeKind::Implements,
                "Class:com.acme.SecuredImpl",
                "Interface:com.acme.api.RemitService",
            ),
            heritage_edge(
                EdgeKind::Implements,
                "Class:com.acme.NonSecuredImpl",
                "Interface:com.acme.api.RemitService",
            ),
        ];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let ids: Vec<&str> = nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Route)
            .map(|n| n.id.as_str())
            .collect();
        let unique: std::collections::HashSet<&&str> = ids.iter().collect();
        assert_eq!(ids.len(), unique.len(), "no duplicate route ids: {ids:?}");
    }

    #[test]
    fn dual_server_bundle_full_ocb_shape() {
        // The full OCB remittance shape: whiteboard /rest/remittance/* + a
        // secured and a non-secured server in one bundle, interface handler.
        let dir = temp_dir("ocb");
        let spring = "custom-remittance/resources/META-INF/spring";
        let mut nodes: Vec<Node> = server_and_bean("/v1", "securedImpl", "com.vpb.RemitImpl")
            .into_iter()
            .map(|n| at_file(n, &format!("{spring}/beans_rest.xml")))
            .collect();
        nodes.extend(
            server_and_bean("/ns/v1", "nsImpl", "com.vpb.NsRemitImpl")
                .into_iter()
                .map(|n| at_file(n, &format!("{spring}/beans_rest.xml"))),
        );
        nodes.push(at_file(
            integration_route(
                "/rest/remittance/*",
                "osgi_servlet",
                serde_json::json!({ "servlet_pattern": "/rest/remittance/*" }),
            ),
            &format!("{spring}/beans_rest_web_servlets.xml"),
        ));
        nodes.push(class_node("com.vpb.RemitImpl"));
        nodes.push(class_node("com.vpb.NsRemitImpl"));
        nodes.push(interface_node("com.vpb.api.RemittanceService"));
        let handler = "com.vpb.api.RemittanceService#getBeneficiaries/0";
        nodes.push(route_node("GET", "/beneficiaries", handler));
        let mut edges = vec![
            handles_route_edge(handler, "GET", "/beneficiaries"),
            heritage_edge(
                EdgeKind::Implements,
                "Class:com.vpb.RemitImpl",
                "Interface:com.vpb.api.RemittanceService",
            ),
            heritage_edge(
                EdgeKind::Implements,
                "Class:com.vpb.NsRemitImpl",
                "Interface:com.vpb.api.RemittanceService",
            ),
        ];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let routes: Vec<&Node> = nodes.iter().filter(|n| n.kind == NodeKind::Route).collect();
        let mut paths: Vec<&str> = routes.iter().filter_map(|n| prop(n, "path")).collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                "/rest/remittance/ns/v1/beneficiaries",
                "/rest/remittance/v1/beneficiaries"
            ]
        );
        assert!(routes
            .iter()
            .all(|n| prop(n, "servlet_prefix_source") == Some("osgi_whiteboard")));
    }

    #[test]
    fn per_bundle_servlet_prefix_selected_by_directory() {
        let dir = temp_dir("bundles");
        let mut nodes = bundle("custom-a", "/rest/a/*", "/v1", "aImpl", "com.acme.a.AImpl");
        nodes.extend(bundle("custom-b", "/rest/b/*", "/v1", "bImpl", "com.acme.b.BImpl"));
        nodes.push(route_node("GET", "/x", "com.acme.a.AImpl#x/0"));
        nodes.push(route_node("GET", "/y", "com.acme.b.BImpl#y/0"));
        let mut edges = vec![
            handles_route_edge("com.acme.a.AImpl#x/0", "GET", "/x"),
            handles_route_edge("com.acme.b.BImpl#y/0", "GET", "/y"),
        ];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let paths: Vec<&str> = nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Route)
            .filter_map(|n| prop(n, "path"))
            .collect();
        assert!(paths.contains(&"/rest/a/v1/x"), "paths: {paths:?}");
        assert!(paths.contains(&"/rest/b/v1/y"), "paths: {paths:?}");
        assert!(nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Route)
            .all(|n| prop(n, "servlet_prefix_source") == Some("osgi_whiteboard")));
    }

    #[test]
    fn single_osgi_servlet_applies_across_directories() {
        // A lone whiteboard pattern still applies repo-wide even when it shares
        // no directory with the server (single-bundle repos, root-level XML).
        let dir = temp_dir("lone");
        let mut nodes = server_and_bean("/v1", "impl", "com.acme.Impl")
            .into_iter()
            .map(|n| at_file(n, "app/config/beans_rest.xml"))
            .collect::<Vec<_>>();
        nodes.push(at_file(
            integration_route(
                "/rest/*",
                "osgi_servlet",
                serde_json::json!({ "servlet_pattern": "/rest/*" }),
            ),
            "web/servlets.xml",
        ));
        nodes.push(route_node("GET", "/x", "com.acme.Impl#x/0"));
        let mut edges = vec![handles_route_edge("com.acme.Impl#x/0", "GET", "/x")];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(prop(route, "path"), Some("/rest/v1/x"));
        assert_eq!(
            prop(route, "servlet_prefix_source"),
            Some("osgi_whiteboard")
        );
    }

    #[test]
    fn multiple_unrelated_osgi_servlets_do_not_cross_apply() {
        // Two bundles declare patterns; a server in a THIRD bundle must not
        // inherit either one (previously: first-node-wins repo-wide).
        let dir = temp_dir("unrelated");
        let mut nodes = bundle("custom-a", "/rest/a/*", "/v1", "aImpl", "com.acme.a.AImpl");
        nodes.extend(bundle("custom-b", "/rest/b/*", "/v1", "bImpl", "com.acme.b.BImpl"));
        nodes.extend(
            server_and_bean("/v1", "cImpl", "com.acme.c.CImpl")
                .into_iter()
                .map(|n| at_file(n, "custom-c/resources/META-INF/spring/beans_rest.xml")),
        );
        nodes.push(route_node("GET", "/z", "com.acme.c.CImpl#z/0"));
        let mut edges = vec![handles_route_edge("com.acme.c.CImpl#z/0", "GET", "/z")];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let route = nodes
            .iter()
            .find(|n| n.kind == NodeKind::Route && prop(n, "handler") == Some("com.acme.c.CImpl#z/0"))
            .unwrap();
        assert_eq!(prop(route, "path"), Some("/v1/z"));
        assert_eq!(prop(route, "servlet_prefix_source"), Some("none"));
    }

    #[test]
    fn config_override_beats_per_bundle_pattern() {
        let dir = temp_dir("cfgwins");
        let mut nodes = bundle("custom-a", "/rest/a/*", "/v1", "aImpl", "com.acme.a.AImpl");
        nodes.push(route_node("GET", "/x", "com.acme.a.AImpl#x/0"));
        let mut edges = vec![handles_route_edge("com.acme.a.AImpl#x/0", "GET", "/x")];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, Some("/api"));
        std::fs::remove_dir_all(&dir).ok();

        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(prop(route, "path"), Some("/api/v1/x"));
        assert_eq!(prop(route, "servlet_prefix_source"), Some("config"));
    }

    #[test]
    fn servlet_prefix_tie_breaks_deterministically() {
        // Two patterns equidistant from the server (same shared directory
        // depth): the lexicographically-first shortest file wins.
        let dir = temp_dir("ties");
        let mut nodes = server_and_bean("/v1", "impl", "com.acme.Impl")
            .into_iter()
            .map(|n| at_file(n, "app/spring/beans_rest.xml"))
            .collect::<Vec<_>>();
        for (file, pattern) in [
            ("app/z/servlets.xml", "/rest/z/*"),
            ("app/a/servlets.xml", "/rest/a/*"),
        ] {
            nodes.push(at_file(
                integration_route(
                    pattern,
                    "osgi_servlet",
                    serde_json::json!({ "servlet_pattern": pattern }),
                ),
                file,
            ));
        }
        nodes.push(route_node("GET", "/x", "com.acme.Impl#x/0"));
        let mut edges = vec![handles_route_edge("com.acme.Impl#x/0", "GET", "/x")];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        // Both score 1 ("app"); files have equal length → lexicographic first.
        assert_eq!(prop(route, "path"), Some("/rest/a/v1/x"));
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
        let mut nodes = server_and_bean(
            "/v1/services",
            "restServiceEndPointImpl",
            " com.acme.RestServiceEndPointImpl",
        );
        nodes.push(integration_route(
            "/rest/*",
            "osgi_servlet",
            serde_json::json!({ "servlet_pattern": "/rest/*" }),
        ));
        let handler = "com.acme.RestServiceEndPointImpl#onOffVoice/1";
        nodes.push(route_node("POST", "/sound-box/on-off-voice", handler));
        let mut edges = vec![handles_route_edge(
            handler,
            "POST",
            "/sound-box/on-off-voice",
        )];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        let full = "/rest/v1/services/sound-box/on-off-voice";
        assert_eq!(prop(route, "path"), Some(full));
        assert_eq!(route.id.as_str(), &format!("Route:POST {full}"));
        assert_eq!(prop(route, "local_path"), Some("/sound-box/on-off-voice"));
        assert_eq!(
            prop(route, "servlet_prefix_source"),
            Some("osgi_whiteboard")
        );

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
        let mut nodes = server_and_bean("/v1/services", "impl", "com.acme.RestServiceEndPointImpl");
        let handler = "com.acme.RestServiceEndPointImpl#onOffVoice/1";
        nodes.push(route_node("POST", "/sound-box/on-off-voice", handler));
        let mut edges = vec![handles_route_edge(
            handler,
            "POST",
            "/sound-box/on-off-voice",
        )];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(
            prop(route, "path"),
            Some("/v1/services/sound-box/on-off-voice")
        );
        assert_eq!(prop(route, "servlet_prefix_source"), Some("none"));
    }

    #[test]
    fn stitch_no_matching_route_is_noop() {
        let dir = temp_dir("nomatch");
        let mut nodes = server_and_bean("/v1/services", "impl", "com.acme.RestServiceEndPointImpl");
        // A route on an unrelated class — must not be rewritten.
        nodes.push(route_node(
            "GET",
            "/other",
            "com.acme.OtherController#get/0",
        ));
        let mut edges = vec![handles_route_edge(
            "com.acme.OtherController#get/0",
            "GET",
            "/other",
        )];

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

    fn class_node(fqcn: &str) -> Node {
        Node {
            id: NodeId::new(format!("Class:{fqcn}")),
            kind: NodeKind::Class,
            name: fqcn.rsplit('.').next().unwrap_or(fqcn).to_string(),
            qualified_name: Some(fqcn.to_string()),
            file: "com/acme/X.java".to_string(),
            range: Range::default(),
            props: None,
        }
    }

    #[test]
    fn simple_name_class_resolves_to_unique_fqcn() {
        let dir = temp_dir("simple");
        // bean `class` is a bare simple name, resolved via the unique Class node in the graph.
        let mut nodes = server_and_bean("/crm", "customerSvc", "CustomerService");
        nodes.push(class_node("com.acme.CustomerService"));
        let handler = "com.acme.CustomerService#getCustomer/1";
        nodes.push(route_node("GET", "/customers/{id}", handler));
        let mut edges = vec![handles_route_edge(handler, "GET", "/customers/{id}")];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(prop(route, "path"), Some("/crm/customers/{id}"));
    }

    #[test]
    fn ambiguous_simple_name_is_not_resolved() {
        let dir = temp_dir("ambig");
        let mut nodes = server_and_bean("/crm", "customerSvc", "CustomerService");
        // Two classes share the simple name → ambiguous → left unresolved → no match.
        nodes.push(class_node("com.acme.CustomerService"));
        nodes.push(class_node("com.other.CustomerService"));
        let handler = "com.acme.CustomerService#getCustomer/1";
        nodes.push(route_node("GET", "/customers/{id}", handler));
        let mut edges = vec![handles_route_edge(handler, "GET", "/customers/{id}")];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(
            prop(route, "path"),
            Some("/customers/{id}"),
            "ambiguous name must not stitch"
        );
        assert!(!edges.iter().any(|e| e.reason == "cxf-jaxrs-prefix"));
    }

    #[test]
    fn emits_bean_to_class_edge() {
        let dir = temp_dir("beanedge");
        let mut nodes = server_and_bean("/crm", "customerSvc", "com.acme.CustomerService");
        nodes.push(class_node("com.acme.CustomerService"));
        let handler = "com.acme.CustomerService#getCustomer/1";
        nodes.push(route_node("GET", "/customers/{id}", handler));
        let mut edges = vec![handles_route_edge(handler, "GET", "/customers/{id}")];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let edge = edges
            .iter()
            .find(|e| e.reason == "cxf-bean-class")
            .expect("bean → Class registration edge expected");
        assert_eq!(edge.kind, EdgeKind::IntegrationLink);
        assert_eq!(edge.src.as_str(), "IntegrationRoute:spring_xml:customerSvc");
        assert_eq!(edge.dst.as_str(), "Class:com.acme.CustomerService");
    }

    #[test]
    fn no_class_node_means_no_bean_class_edge() {
        let dir = temp_dir("noclass");
        // FQCN bean class, but the class isn't a graph node (e.g. not indexed).
        let mut nodes = server_and_bean("/crm", "customerSvc", "com.acme.CustomerService");
        let handler = "com.acme.CustomerService#getCustomer/1";
        nodes.push(route_node("GET", "/customers/{id}", handler));
        let mut edges = vec![handles_route_edge(handler, "GET", "/customers/{id}")];

        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        // Route is still stitched via the FQCN handler prefix-match …
        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(prop(route, "path"), Some("/crm/customers/{id}"));
        // … but no bean → Class edge, since the class node doesn't exist.
        assert!(!edges.iter().any(|e| e.reason == "cxf-bean-class"));
    }

    // ── normalize_prefix / join_url unit tests ───────────────────────────────

    #[test]
    fn normalize_prefix_variants() {
        assert_eq!(normalize_prefix("/rest/*"), "rest");
        assert_eq!(normalize_prefix("/rest/"), "rest");
        assert_eq!(normalize_prefix("rest"), "rest");
        assert_eq!(normalize_prefix("/api/v1/*"), "api/v1");
        assert_eq!(normalize_prefix("*"), "");
        assert_eq!(normalize_prefix("/"), "");
        assert_eq!(normalize_prefix("  /rest/*  "), "rest");
    }

    #[test]
    fn join_url_variants() {
        assert_eq!(
            join_url(&["rest", "/v1/services", "/a/b"]),
            "/rest/v1/services/a/b"
        );
        assert_eq!(join_url(&["", "/crm", "/x"]), "/crm/x"); // empty servlet prefix collapses
        assert_eq!(join_url(&["/a/", "/b/", "c"]), "/a/b/c"); // dup/trailing slashes normalized
        assert_eq!(join_url(&["", "", ""]), "/");
    }

    // ── servlet-prefix detectors ─────────────────────────────────────────────

    #[test]
    fn servlet_prefix_priority_config_over_whiteboard() {
        let dir = temp_dir("prio");
        let nodes = vec![integration_route(
            "/rest/*",
            "osgi_servlet",
            serde_json::json!({ "servlet_pattern": "/rest/*" }),
        )];
        // config override wins over an osgi_servlet node.
        let out = resolve_servlet_prefix(&dir, &nodes, Some("/gateway"));
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(out, Some(("gateway".to_string(), "config")));
    }

    #[test]
    fn servlet_prefix_whiteboard_when_no_config() {
        let dir = temp_dir("wb");
        let nodes = vec![integration_route(
            "/rest/*",
            "osgi_servlet",
            serde_json::json!({ "servlet_pattern": "/rest/*" }),
        )];
        let out = resolve_servlet_prefix(&dir, &nodes, None);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(out, Some(("rest".to_string(), "osgi_whiteboard")));
    }

    #[test]
    fn servlet_prefix_none_when_nothing_declares_one() {
        let dir = temp_dir("nowt");
        let out = resolve_servlet_prefix(&dir, &[], None);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(out, None);
    }

    #[test]
    fn web_xml_picks_cxf_servlet_among_many() {
        let dir = temp_dir("multi-servlet");
        let web = r#"<web-app>
            <servlet>
                <servlet-name>dispatcher</servlet-name>
                <servlet-class>org.springframework.web.servlet.DispatcherServlet</servlet-class>
            </servlet>
            <servlet-mapping><servlet-name>dispatcher</servlet-name><url-pattern>/</url-pattern></servlet-mapping>
            <servlet>
                <servlet-name>cxf</servlet-name>
                <servlet-class>org.apache.cxf.transport.servlet.CXFServlet</servlet-class>
            </servlet>
            <servlet-mapping><servlet-name>cxf</servlet-name><url-pattern>/services/*</url-pattern></servlet-mapping>
        </web-app>"#;
        std::fs::create_dir_all(dir.join("WEB-INF")).unwrap();
        std::fs::write(dir.join("WEB-INF/web.xml"), web).unwrap();
        let out = resolve_servlet_prefix(&dir, &[], None);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(out, Some(("services".to_string(), "web_xml")));
    }

    #[test]
    fn web_xml_servlet_name_mismatch_yields_none() {
        let dir = temp_dir("mismatch");
        // CXFServlet present but its mapping uses a different servlet-name.
        let web = r#"<web-app>
            <servlet>
                <servlet-name>cxf</servlet-name>
                <servlet-class>org.apache.cxf.transport.servlet.CXFServlet</servlet-class>
            </servlet>
            <servlet-mapping><servlet-name>other</servlet-name><url-pattern>/nope/*</url-pattern></servlet-mapping>
        </web-app>"#;
        std::fs::create_dir_all(dir.join("WEB-INF")).unwrap();
        std::fs::write(dir.join("WEB-INF/web.xml"), web).unwrap();
        let out = resolve_servlet_prefix(&dir, &[], None);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(out, None);
    }

    #[test]
    fn spring_boot_properties_cxf_path_forms() {
        for (body, expect) in [
            ("cxf.path=/api", "api"),
            ("cxf.path = /api", "api"),
            ("cxf.path=\"/api\"", "api"),
            ("# cxf.path=/ignored\ncxf.path=/real", "real"),
        ] {
            let dir = temp_dir("props");
            std::fs::write(dir.join("application.properties"), body).unwrap();
            let out = resolve_servlet_prefix(&dir, &[], None);
            std::fs::remove_dir_all(&dir).ok();
            assert_eq!(
                out,
                Some((expect.to_string(), "spring_boot")),
                "body={body:?}"
            );
        }
    }

    #[test]
    fn spring_boot_yaml_nested_and_flat() {
        let dir = temp_dir("yaml-nested");
        std::fs::write(dir.join("application.yml"), "cxf:\n  path: /api\n").unwrap();
        let out = resolve_servlet_prefix(&dir, &[], None);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(out, Some(("api".to_string(), "spring_boot")));

        let dir = temp_dir("yaml-flat");
        std::fs::write(dir.join("application.yml"), "cxf.path: \"/gw\"\n").unwrap();
        let out = resolve_servlet_prefix(&dir, &[], None);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(out, Some(("gw".to_string(), "spring_boot")));
    }

    // ── stitch scenarios ─────────────────────────────────────────────────────

    #[test]
    fn stitch_rewrites_all_routes_of_a_class() {
        let dir = temp_dir("multiroute");
        let mut nodes = server_and_bean("/crm", "svc", "com.acme.Svc");
        nodes.push(route_node("GET", "/customers/{id}", "com.acme.Svc#get/1"));
        nodes.push(route_node("POST", "/customers", "com.acme.Svc#add/1"));
        let mut edges = vec![
            handles_route_edge("com.acme.Svc#get/1", "GET", "/customers/{id}"),
            handles_route_edge("com.acme.Svc#add/1", "POST", "/customers"),
        ];
        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let paths: std::collections::BTreeSet<_> = nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Route)
            .filter_map(|n| prop(n, "path").map(String::from))
            .collect();
        assert!(paths.contains("/crm/customers/{id}"), "paths={paths:?}");
        assert!(paths.contains("/crm/customers"), "paths={paths:?}");
    }

    #[test]
    fn stitch_multiple_servers_route_to_their_own_class() {
        let dir = temp_dir("multiserver");
        let mut nodes = server_and_bean("/crm", "crmSvc", "com.acme.Crm");
        nodes.extend(server_and_bean("/billing", "billSvc", "com.acme.Billing"));
        nodes.push(route_node("GET", "/a", "com.acme.Crm#a/0"));
        nodes.push(route_node("GET", "/b", "com.acme.Billing#b/0"));
        let mut edges = vec![
            handles_route_edge("com.acme.Crm#a/0", "GET", "/a"),
            handles_route_edge("com.acme.Billing#b/0", "GET", "/b"),
        ];
        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let by_handler = |h: &str| {
            nodes
                .iter()
                .find(|n| n.kind == NodeKind::Route && prop(n, "handler") == Some(h))
                .and_then(|n| prop(n, "path").map(String::from))
        };
        assert_eq!(by_handler("com.acme.Crm#a/0").as_deref(), Some("/crm/a"));
        assert_eq!(
            by_handler("com.acme.Billing#b/0").as_deref(),
            Some("/billing/b")
        );
    }

    #[test]
    fn stitch_preserves_existing_class_level_prefix() {
        // Route.path already carries a class-level @Path ("/customerservice"); stitch prepends only.
        let dir = temp_dir("classprefix");
        let mut nodes = server_and_bean("/crm", "svc", "com.acme.Svc");
        nodes.push(route_node(
            "GET",
            "/customerservice/customers/{id}",
            "com.acme.Svc#get/1",
        ));
        let mut edges = vec![handles_route_edge(
            "com.acme.Svc#get/1",
            "GET",
            "/customerservice/customers/{id}",
        )];
        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();
        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(
            prop(route, "path"),
            Some("/crm/customerservice/customers/{id}")
        );
    }

    #[test]
    fn stitch_blueprint_source_bean_resolves() {
        let dir = temp_dir("bp");
        // Blueprint bean node (source blueprint_xml) + component-id-style ref via the same id.
        let mut nodes = vec![
            integration_route(
                "/api",
                "cxf_jaxrs_server",
                serde_json::json!({ "address": "/api", "beans": ["svc"] }),
            ),
            integration_route(
                "svc",
                "blueprint_xml",
                serde_json::json!({ "class": "com.acme.Bp" }),
            ),
        ];
        nodes.push(route_node("GET", "/x", "com.acme.Bp#x/0"));
        let mut edges = vec![handles_route_edge("com.acme.Bp#x/0", "GET", "/x")];
        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();
        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(prop(route, "path"), Some("/api/x"));
    }

    #[test]
    fn stitch_inline_service_bean() {
        // Anonymous inline serviceBean: class travels on the server via `bean_classes` (no ref/id).
        let dir = temp_dir("inline");
        let mut nodes = vec![integration_route(
            "/api",
            "cxf_jaxrs_server",
            serde_json::json!({ "address": "/api", "beans": [], "bean_classes": ["com.acme.Inline"] }),
        )];
        nodes.push(class_node("com.acme.Inline"));
        nodes.push(route_node("GET", "/x", "com.acme.Inline#x/0"));
        let mut edges = vec![handles_route_edge("com.acme.Inline#x/0", "GET", "/x")];
        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();

        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(prop(route, "path"), Some("/api/x"));
        // Inline bean has no bean node — the registration edge originates at the server node.
        let edge = edges
            .iter()
            .find(|e| e.reason == "cxf-bean-class")
            .expect("server → Class edge for inline bean");
        assert_eq!(edge.src.as_str(), "IntegrationRoute:cxf_jaxrs_server:/api");
        assert_eq!(edge.dst.as_str(), "Class:com.acme.Inline");
    }

    #[test]
    fn stitch_inline_bean_simple_name_resolves() {
        let dir = temp_dir("inline-simple");
        let mut nodes = vec![integration_route(
            "/api",
            "cxf_jaxrs_server",
            serde_json::json!({ "address": "/api", "beans": [], "bean_classes": ["Inline"] }),
        )];
        nodes.push(class_node("com.acme.Inline"));
        nodes.push(route_node("GET", "/x", "com.acme.Inline#x/0"));
        let mut edges = vec![handles_route_edge("com.acme.Inline#x/0", "GET", "/x")];
        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();
        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(
            prop(route, "path"),
            Some("/api/x"),
            "simple inline class should resolve"
        );
    }

    #[test]
    fn bean_class_edge_deduped_when_bean_shared_by_two_servers() {
        let dir = temp_dir("dedup");
        let mut nodes = vec![
            integration_route(
                "/a",
                "cxf_jaxrs_server",
                serde_json::json!({ "address": "/a", "beans": ["svc"] }),
            ),
            integration_route(
                "/b",
                "cxf_jaxrs_server",
                serde_json::json!({ "address": "/b", "beans": ["svc"] }),
            ),
            integration_route(
                "svc",
                "spring_xml",
                serde_json::json!({ "class": "com.acme.Svc" }),
            ),
            class_node("com.acme.Svc"),
        ];
        nodes.push(route_node("GET", "/x", "com.acme.Svc#x/0"));
        let mut edges = vec![handles_route_edge("com.acme.Svc#x/0", "GET", "/x")];
        stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
        std::fs::remove_dir_all(&dir).ok();
        let bean_class_edges = edges
            .iter()
            .filter(|e| e.reason == "cxf-bean-class")
            .count();
        assert_eq!(
            bean_class_edges, 1,
            "bean → Class edge must be deduped across servers"
        );
    }
}
