use cih_core::{Edge, EdgeKind, Node, NodeKind};
use std::collections::{BTreeMap, BTreeSet};

pub struct ProcessStep {
    pub process_id: String,
    pub step_number: usize,
    pub symbol: Node,
}

#[derive(Clone, Debug)]
pub struct DbTableAccess {
    pub table_name: String,
    pub reads: bool,
    pub writes: bool,
}

/// `(name, type)` pairs, e.g. `("OrderCreatedEvent", "kafka")`.
pub type MessagingPairs = Vec<(String, String)>;

pub struct WikiGraph {
    pub nodes_by_id: BTreeMap<String, Node>,
    pub community_nodes: Vec<Node>,
    pub process_nodes: Vec<Node>,

    /// community_id → member nodes (sorted by name)
    pub members_by_community: BTreeMap<String, Vec<Node>>,
    /// symbol_id → community_id
    pub community_by_member: BTreeMap<String, String>,

    pub calls_out: BTreeMap<String, Vec<String>>,
    pub calls_in: BTreeMap<String, Vec<String>>,

    pub tests_out: BTreeMap<String, Vec<String>>,
    pub tests_in: BTreeMap<String, Vec<String>>,

    pub external_calls: BTreeMap<String, Vec<String>>,
    pub publishes: BTreeMap<String, Vec<String>>,
    pub listens: BTreeMap<String, Vec<String>>,

    /// (handler_method_node, route_node) sorted by path then http_method
    pub routes: Vec<(Node, Node)>,

    /// process_id → steps sorted by step_number, then symbol id
    pub process_steps: BTreeMap<String, Vec<ProcessStep>>,

    pub community_routes: BTreeMap<String, Vec<(Node, Node)>>,
    /// community_id → list of test node ids that cover any member
    pub community_tests: BTreeMap<String, Vec<String>>,
    pub community_class_counts: BTreeMap<String, usize>,
    pub community_method_counts: BTreeMap<String, usize>,
    /// community_id → stereotype → count
    pub community_stereotypes: BTreeMap<String, BTreeMap<String, usize>>,
    /// (src_community_id, dst_community_id, call_count) sorted by (src, dst)
    pub inter_community_calls: Vec<(String, String, usize)>,

    /// class_id → method/constructor nodes (from HasMethod edges), sorted by start_line
    pub methods_by_class: BTreeMap<String, Vec<Node>>,

    /// interface_method_id → [impl_method_ids] (reverse of MethodImplements edges)
    /// Used in `build_call_chain` to continue traversal through interface boundaries.
    pub impl_methods: BTreeMap<String, Vec<String>>,

    /// method_id → [dbquery_id]
    pub executes_query: BTreeMap<String, Vec<String>>,
    /// dbquery_id → [dbtable_id]
    pub query_reads_table: BTreeMap<String, Vec<String>>,
    /// dbquery_id → [dbtable_id]
    pub query_writes_table: BTreeMap<String, Vec<String>>,
    /// community_id → sorted unique tables accessed (reads + writes combined)
    pub community_db_tables: BTreeMap<String, Vec<DbTableAccess>>,

    /// controller_class_name → [(handler_method, route_node)] sorted by path/method
    pub routes_by_controller: BTreeMap<String, Vec<(Node, Node)>>,
    /// controller_class_name → feature slug
    pub controller_feature: BTreeMap<String, String>,
}

fn controller_name_from_handler_id(handler_id: &str) -> &str {
    let without_kind = handler_id.strip_prefix("Method:").unwrap_or(handler_id);
    let fqcn = without_kind.split('#').next().unwrap_or(without_kind);
    fqcn.rsplit('.').next().unwrap_or(fqcn)
}

fn feature_from_file_path(file: &str) -> String {
    // Strategy 1: explicit modules/<feature>/ segment (e.g. com.example.modules.order)
    if let Some(start) = file.find("modules/") {
        let rest = &file[start + "modules/".len()..];
        if let Some(end) = rest.find('/') {
            if end > 0 {
                return rest[..end].to_string();
            }
        }
    }

    // Strategy 2: Maven multi-module layout — extract the root module dir (the part
    // before /src/main/java/ or /src/main/kotlin/) and normalise it.
    //   banking-overdraft/src/...       → "overdraft"
    //   banking-overdraft-api/src/...   → "overdraft"
    //   custom-impl/src/.../overdraft/  → fallthrough to strategy 3
    let src_markers = ["/src/main/java/", "/src/main/kotlin/",
                        "/src/test/java/",  "/src/test/kotlin/"];
    if let Some(marker_pos) = src_markers.iter().find_map(|m| file.find(m)) {
        let module_dir = &file[..marker_pos];
        // Keep only the last path segment (the Maven module name itself)
        let module_name = module_dir.rsplit('/').next().unwrap_or(module_dir);
        if !module_name.is_empty() {
            let normalised = normalise_module_name(module_name);
            // Only use the normalised module name when it resolves to something
            // domain-specific.  Bare catch-all module names like "core", "common",
            // "custom", "impl" carry no feature meaning — fall through to strategy 3.
            const STILL_GENERIC: &[&str] = &[
                "core", "common", "shared", "base", "impl",
                "custom", "default", "generic", "abstract",
                "infra", "infrastructure", "platform",
            ];
            if !normalised.is_empty()
                && normalised != "shared"
                && !STILL_GENERIC.contains(&normalised.as_str())
            {
                return normalised;
            }
        }

        // Strategy 3 (fallback): read the deepest meaningful segment of the Java
        // package path after src/main/java/.
        for marker in &src_markers {
            if let Some(pos) = file.find(marker) {
                let pkg_path = &file[pos + marker.len()..];
                // Drop the filename (last segment) — we want the package, not the class.
                let pkg_dir = match pkg_path.rfind('/') {
                    Some(p) => &pkg_path[..p],
                    None => pkg_path,
                };
                if let Some(feat) = meaningful_package_feature(pkg_dir) {
                    return feat;
                }
                break;
            }
        }
    }

    "shared".to_string()
}

/// Strip well-known project prefixes and suffixes from a Maven module name.
/// Returns a normalised feature slug, or empty string if no useful name remains.
///
/// Examples:
///   "banking-overdraft"      → "overdraft"
///   "banking-overdraft-api"  → "overdraft"
///   "payment-service-impl"   → "payment"
///   "my-app-core"            → (empty — too generic, handled as catch-all)
fn normalise_module_name(name: &str) -> String {
    const GENERIC_SUFFIXES: &[&str] = &[
        "-api", "-service", "-impl", "-core", "-common", "-module",
        "-lib", "-client", "-server", "-domain", "-model", "-dto",
        "-web", "-rest", "-grpc",
    ];
    const GENERIC_PREFIXES: &[&str] = &[
        "banking-", "payment-", "finance-", "base-", "common-",
        "core-", "shared-", "platform-", "infra-", "infrastructure-",
        "app-", "service-",
    ];

    let mut s = name.to_lowercase();

    // Strip suffixes (order matters — strip longest first)
    let mut changed = true;
    while changed {
        changed = false;
        for suf in GENERIC_SUFFIXES {
            if s.len() > suf.len() && s.ends_with(suf) {
                s.truncate(s.len() - suf.len());
                changed = true;
            }
        }
    }

    // Strip prefixes
    changed = true;
    while changed {
        changed = false;
        for pfx in GENERIC_PREFIXES {
            if s.len() > pfx.len() && s.starts_with(pfx) {
                s = s[pfx.len()..].to_string();
                changed = true;
            }
        }
    }

    // Reject single-character or purely numeric leftovers
    if s.len() <= 1 || s.chars().all(|c| c.is_ascii_digit()) {
        return String::new();
    }

    s
}

/// Walk a Java package path (e.g. "com/example/bank/overdraft/service") from right
/// to left and return the first segment that isn't a generic Java package name.
fn meaningful_package_feature(pkg_dir: &str) -> Option<String> {
    const SKIP: &[&str] = &[
        // language / build dirs
        "java", "kotlin", "scala", "groovy",
        "main", "test",
        // top-level TLDs and common org segments (skip first 1-2 segments after these)
        "com", "org", "net", "io", "co", "dev",
        // cross-cutting technical layers
        "impl", "internal", "common", "shared", "core", "base", "custom", "default",
        "util", "utils", "helper", "helpers", "support",
        "model", "models", "dto", "dtos", "entity", "entities", "domain",
        "config", "configuration", "properties",
        "service", "services", "usecase", "usecases",
        "repository", "repositories", "repo", "repos", "persistence",
        "controller", "controllers", "handler", "handlers", "resource", "resources",
        "exception", "exceptions", "error", "errors",
        "web", "rest", "grpc", "api", "client", "server",
        "messaging", "event", "events", "listener", "listeners",
        "gateway", "adapter", "adapters", "infrastructure", "infra",
        "security", "filter", "filters", "interceptor", "interceptors",
        "mapper", "mappers", "converter", "converters",
        "scheduler", "job", "jobs", "task", "tasks",
    ];

    for segment in pkg_dir.split('/').rev() {
        let seg = segment.trim();
        if seg.is_empty() || seg.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if SKIP.contains(&seg) {
            continue;
        }
        // Skip single letters (e.g. "v" from versioning) and pure version segments
        if seg.len() <= 1 {
            continue;
        }
        let mut chars = seg.chars();
        if matches!(chars.next(), Some('v') | Some('V')) && chars.all(|c| c.is_ascii_digit()) {
            continue;
        }
        return Some(seg.to_string());
    }
    None
}

// ── Shared helpers ───────────────────────────────────────────────────────────

struct EdgeIndex {
    calls_out: BTreeMap<String, Vec<String>>,
    calls_in: BTreeMap<String, Vec<String>>,
    tests_out: BTreeMap<String, Vec<String>>,
    tests_in: BTreeMap<String, Vec<String>>,
    external_calls: BTreeMap<String, Vec<String>>,
    publishes: BTreeMap<String, Vec<String>>,
    listens: BTreeMap<String, Vec<String>>,
    routes: Vec<(Node, Node)>,
    methods_by_class: BTreeMap<String, Vec<Node>>,
    executes_query: BTreeMap<String, Vec<String>>,
    query_reads_table: BTreeMap<String, Vec<String>>,
    query_writes_table: BTreeMap<String, Vec<String>>,
    impl_methods: BTreeMap<String, Vec<String>>,
}

fn index_edges(edges: &[Edge], nodes_by_id: &BTreeMap<String, Node>) -> EdgeIndex {
    let mut calls_out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut calls_in: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut tests_out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut tests_in: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut external_calls: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut publishes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut listens: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut routes: Vec<(Node, Node)> = Vec::new();
    let mut methods_by_class: BTreeMap<String, Vec<Node>> = BTreeMap::new();
    let mut executes_query: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut query_reads_table: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut query_writes_table: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut impl_methods: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for e in edges {
        let src = e.src.as_str().to_string();
        let dst = e.dst.as_str().to_string();
        match e.kind {
            EdgeKind::Calls => {
                calls_out.entry(src.clone()).or_default().push(dst.clone());
                calls_in.entry(dst).or_default().push(src);
            }
            EdgeKind::Tests => {
                tests_out.entry(src.clone()).or_default().push(dst.clone());
                tests_in.entry(dst).or_default().push(src);
            }
            EdgeKind::ExternalCall => {
                external_calls.entry(src).or_default().push(dst);
            }
            EdgeKind::PublishesEvent => {
                publishes.entry(src).or_default().push(dst);
            }
            EdgeKind::ListensTo => {
                listens.entry(src).or_default().push(dst);
            }
            EdgeKind::HandlesRoute => {
                if let (Some(handler), Some(route)) =
                    (nodes_by_id.get(&src), nodes_by_id.get(&dst))
                {
                    routes.push((handler.clone(), route.clone()));
                }
            }
            EdgeKind::HasMethod => {
                if let Some(method_node) = nodes_by_id.get(&dst) {
                    methods_by_class
                        .entry(src)
                        .or_default()
                        .push(method_node.clone());
                }
            }
            // MethodImplements: src=impl_method, dst=interface_method.
            // Build the reverse map so BFS can hop from interface → impl.
            EdgeKind::MethodImplements => {
                impl_methods.entry(dst).or_default().push(src);
            }
            EdgeKind::ExecutesQuery => {
                executes_query.entry(src).or_default().push(dst);
            }
            EdgeKind::ReadsTable => {
                query_reads_table.entry(src).or_default().push(dst);
            }
            EdgeKind::WritesTable => {
                query_writes_table.entry(src).or_default().push(dst);
            }
            _ => {}
        }
    }

    for methods in methods_by_class.values_mut() {
        methods.sort_by_key(|m| m.range.start_line);
    }
    routes.sort_by(|(_, r1), (_, r2)| {
        route_path(r1)
            .cmp(&route_path(r2))
            .then(route_http_method(r1).cmp(&route_http_method(r2)))
    });

    EdgeIndex {
        calls_out, calls_in, tests_out, tests_in,
        external_calls, publishes, listens, routes,
        methods_by_class, executes_query, query_reads_table,
        query_writes_table, impl_methods,
    }
}

struct CommunityStats {
    community_routes: BTreeMap<String, Vec<(Node, Node)>>,
    community_tests: BTreeMap<String, Vec<String>>,
    community_class_counts: BTreeMap<String, usize>,
    community_method_counts: BTreeMap<String, usize>,
    community_stereotypes: BTreeMap<String, BTreeMap<String, usize>>,
    inter_community_calls: Vec<(String, String, usize)>,
    community_db_tables: BTreeMap<String, Vec<DbTableAccess>>,
}

/// Derive per-community stats from membership maps and an edge index.
///
/// `extra_db_nodes` handles nodes (e.g. JPA @Entity classes in package mode) that are
/// not in `members_by_community` but still carry `ExecutesQuery` edges. Pass `&[]` when
/// not needed. `node_comm_id` maps such a node to its community id string.
#[allow(clippy::too_many_arguments)] // page-renderer context bundle; refactor tracked with wiki rework
fn derive_community_stats(
    members_by_community: &BTreeMap<String, Vec<Node>>,
    community_by_member: &BTreeMap<String, String>,
    calls_out: &BTreeMap<String, Vec<String>>,
    tests_in: &BTreeMap<String, Vec<String>>,
    routes: &[(Node, Node)],
    executes_query: &BTreeMap<String, Vec<String>>,
    query_reads_table: &BTreeMap<String, Vec<String>>,
    query_writes_table: &BTreeMap<String, Vec<String>>,
    extra_db_nodes: &[Node],
    node_comm_id: impl Fn(&Node) -> String,
) -> CommunityStats {
    let mut community_routes: BTreeMap<String, Vec<(Node, Node)>> = BTreeMap::new();
    let mut community_class_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut community_method_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut community_stereotypes: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut cross: BTreeMap<(String, String), usize> = BTreeMap::new();

    for (comm_id, members) in members_by_community {
        let mut classes = 0usize;
        let mut methods = 0usize;
        let mut stereo: BTreeMap<String, usize> = BTreeMap::new();
        for m in members {
            match m.kind {
                NodeKind::Class
                | NodeKind::Interface
                | NodeKind::Enum
                | NodeKind::Record
                | NodeKind::Annotation => {
                    classes += 1;
                    if let Some(s) = node_stereotype(m) {
                        *stereo.entry(s.to_string()).or_insert(0) += 1;
                    }
                }
                NodeKind::Method | NodeKind::Function | NodeKind::Constructor => {
                    methods += 1;
                }
                _ => {}
            }
        }
        community_class_counts.insert(comm_id.clone(), classes);
        community_method_counts.insert(comm_id.clone(), methods);
        if !stereo.is_empty() {
            community_stereotypes.insert(comm_id.clone(), stereo);
        }
    }

    for (handler, route) in routes {
        if let Some(comm_id) = community_by_member.get(handler.id.as_str()) {
            community_routes
                .entry(comm_id.clone())
                .or_default()
                .push((handler.clone(), route.clone()));
        }
    }

    let mut community_tests_set: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (comm_id, members) in members_by_community {
        for m in members {
            if let Some(testers) = tests_in.get(m.id.as_str()) {
                for t in testers {
                    community_tests_set
                        .entry(comm_id.clone())
                        .or_default()
                        .insert(t.clone());
                }
            }
        }
    }
    let community_tests: BTreeMap<String, Vec<String>> = community_tests_set
        .into_iter()
        .map(|(k, v)| (k, v.into_iter().collect()))
        .collect();

    for (src_id, dst_ids) in calls_out {
        if let Some(src_comm) = community_by_member.get(src_id) {
            for dst_id in dst_ids {
                if let Some(dst_comm) = community_by_member.get(dst_id) {
                    if src_comm != dst_comm {
                        *cross
                            .entry((src_comm.clone(), dst_comm.clone()))
                            .or_insert(0) += 1;
                    }
                }
            }
        }
    }
    let inter_community_calls: Vec<(String, String, usize)> =
        cross.into_iter().map(|((a, b), c)| (a, b, c)).collect();

    let mut raw_db: BTreeMap<String, BTreeMap<String, (bool, bool)>> = BTreeMap::new();
    for (comm_id, members) in members_by_community {
        for member in members {
            if let Some(query_ids) = executes_query.get(member.id.as_str()) {
                for qid in query_ids {
                    for tid in query_reads_table.get(qid.as_str()).into_iter().flatten() {
                        let name = tid.strip_prefix("DbTable:").unwrap_or(tid).to_string();
                        raw_db.entry(comm_id.clone()).or_default().entry(name).or_default().0 = true;
                    }
                    for tid in query_writes_table.get(qid.as_str()).into_iter().flatten() {
                        let name = tid.strip_prefix("DbTable:").unwrap_or(tid).to_string();
                        raw_db.entry(comm_id.clone()).or_default().entry(name).or_default().1 = true;
                    }
                }
            }
        }
    }
    // Class/Record/Interface nodes not in members_by_community (e.g. JPA @Entity classes in
    // package mode) can still carry ExecutesQuery edges. Walk them separately.
    for node in extra_db_nodes {
        if !matches!(node.kind, NodeKind::Class | NodeKind::Interface | NodeKind::Record) {
            continue;
        }
        let Some(query_ids) = executes_query.get(node.id.as_str()) else { continue };
        let comm_id = node_comm_id(node);
        for qid in query_ids {
            for tid in query_reads_table.get(qid.as_str()).into_iter().flatten() {
                let name = tid.strip_prefix("DbTable:").unwrap_or(tid).to_string();
                raw_db.entry(comm_id.clone()).or_default().entry(name).or_default().0 = true;
            }
            for tid in query_writes_table.get(qid.as_str()).into_iter().flatten() {
                let name = tid.strip_prefix("DbTable:").unwrap_or(tid).to_string();
                raw_db.entry(comm_id.clone()).or_default().entry(name).or_default().1 = true;
            }
        }
    }

    let community_db_tables: BTreeMap<String, Vec<DbTableAccess>> = raw_db
        .into_iter()
        .map(|(comm_id, tables)| {
            let mut v: Vec<DbTableAccess> = tables
                .into_iter()
                .map(|(name, (r, w))| DbTableAccess { table_name: name, reads: r, writes: w })
                .collect();
            v.sort_by(|a, b| a.table_name.cmp(&b.table_name));
            (comm_id, v)
        })
        .collect();

    CommunityStats {
        community_routes, community_tests,
        community_class_counts, community_method_counts, community_stereotypes,
        inter_community_calls, community_db_tables,
    }
}

// ── WikiGraph builders ───────────────────────────────────────────────────────

impl WikiGraph {
    pub fn build(
        nodes: &[Node],
        edges: &[Edge],
        community_nodes: &[Node],
        community_edges: &[Edge],
    ) -> Self {
        let mut nodes_by_id: BTreeMap<String, Node> = BTreeMap::new();
        for n in nodes {
            nodes_by_id.insert(n.id.as_str().to_string(), n.clone());
        }
        for n in community_nodes {
            nodes_by_id.insert(n.id.as_str().to_string(), n.clone());
        }

        let mut comm_vec: Vec<Node> = community_nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Community)
            .cloned()
            .collect();
        comm_vec.sort_by(|a, b| a.name.cmp(&b.name));

        let mut proc_vec: Vec<Node> = community_nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Process)
            .cloned()
            .collect();
        proc_vec.sort_by(|a, b| a.name.cmp(&b.name));

        let mut members_by_community: BTreeMap<String, Vec<Node>> = BTreeMap::new();
        let mut community_by_member: BTreeMap<String, String> = BTreeMap::new();
        let mut steps_raw: BTreeMap<String, Vec<(usize, Node)>> = BTreeMap::new();

        for e in community_edges {
            match e.kind {
                EdgeKind::MemberOf => {
                    let member_id = e.src.as_str().to_string();
                    let comm_id = e.dst.as_str().to_string();
                    community_by_member.insert(member_id.clone(), comm_id.clone());
                    if let Some(member_node) = nodes_by_id.get(&member_id) {
                        members_by_community
                            .entry(comm_id)
                            .or_default()
                            .push(member_node.clone());
                    }
                }
                EdgeKind::StepInProcess => {
                    let symbol_id = e.src.as_str().to_string();
                    let proc_id = e.dst.as_str().to_string();
                    let step_num = e
                        .reason
                        .strip_prefix("step:")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(usize::MAX);
                    if let Some(sym) = nodes_by_id.get(&symbol_id) {
                        steps_raw.entry(proc_id).or_default().push((step_num, sym.clone()));
                    }
                }
                _ => {}
            }
        }

        for members in members_by_community.values_mut() {
            members.sort_by(|a, b| a.name.cmp(&b.name));
        }

        let mut process_steps: BTreeMap<String, Vec<ProcessStep>> = BTreeMap::new();
        for (proc_id, mut raw) in steps_raw {
            raw.sort_by(|(n1, s1), (n2, s2)| n1.cmp(n2).then(s1.id.as_str().cmp(s2.id.as_str())));
            let steps = raw
                .into_iter()
                .map(|(step_number, symbol)| ProcessStep {
                    process_id: proc_id.clone(),
                    step_number,
                    symbol,
                })
                .collect();
            process_steps.insert(proc_id, steps);
        }

        let idx = index_edges(edges, &nodes_by_id);

        let mut routes_by_controller: BTreeMap<String, Vec<(Node, Node)>> = BTreeMap::new();
        let mut controller_feature: BTreeMap<String, String> = BTreeMap::new();
        for (handler, route) in &idx.routes {
            let ctrl = controller_name_from_handler_id(handler.id.as_str()).to_string();
            routes_by_controller
                .entry(ctrl.clone())
                .or_default()
                .push((handler.clone(), route.clone()));
            controller_feature
                .entry(ctrl)
                .or_insert_with(|| feature_from_file_path(&handler.file));
        }

        let stats = derive_community_stats(
            &members_by_community,
            &community_by_member,
            &idx.calls_out,
            &idx.tests_in,
            &idx.routes,
            &idx.executes_query,
            &idx.query_reads_table,
            &idx.query_writes_table,
            &[],
            |_| String::new(),
        );

        let EdgeIndex {
            calls_out, calls_in, tests_out, tests_in,
            external_calls, publishes, listens, routes,
            methods_by_class, executes_query, query_reads_table,
            query_writes_table, impl_methods,
        } = idx;

        WikiGraph {
            nodes_by_id,
            community_nodes: comm_vec,
            process_nodes: proc_vec,
            members_by_community,
            community_by_member,
            calls_out,
            calls_in,
            tests_out,
            tests_in,
            external_calls,
            publishes,
            listens,
            routes,
            process_steps,
            community_routes: stats.community_routes,
            community_tests: stats.community_tests,
            community_class_counts: stats.community_class_counts,
            community_method_counts: stats.community_method_counts,
            community_stereotypes: stats.community_stereotypes,
            inter_community_calls: stats.inter_community_calls,
            methods_by_class,
            impl_methods,
            executes_query,
            query_reads_table,
            query_writes_table,
            community_db_tables: stats.community_db_tables,
            routes_by_controller,
            controller_feature,
        }
    }

    /// Build a `WikiGraph` grouped by Java package path instead of Leiden communities.
    /// Each `modules/<feature>/` package becomes one synthetic community (`Pkg:<feature>`).
    /// No community artifacts needed — works directly from the graph nodes/edges.
    ///
    /// `feature_of(node_id, file)` maps a node to a feature slug.
    /// When a pre-computed artifact is available, use the node_id for direct lookup;
    /// otherwise fall back to file-path heuristics.
    pub fn build_package_grouped(
        nodes: &[Node],
        edges: &[Edge],
        feature_of: &dyn Fn(&str, &str) -> String,
    ) -> Self {
        let mut nodes_by_id: BTreeMap<String, Node> = BTreeMap::new();
        for n in nodes {
            nodes_by_id.insert(n.id.as_str().to_string(), n.clone());
        }

        let idx = index_edges(edges, &nodes_by_id);

        let mut routes_by_controller: BTreeMap<String, Vec<(Node, Node)>> = BTreeMap::new();
        let mut controller_feature: BTreeMap<String, String> = BTreeMap::new();
        for (handler, route) in &idx.routes {
            let ctrl = controller_name_from_handler_id(handler.id.as_str()).to_string();
            routes_by_controller
                .entry(ctrl.clone())
                .or_default()
                .push((handler.clone(), route.clone()));
            controller_feature
                .entry(ctrl)
                .or_insert_with(|| feature_of(handler.id.as_str(), &handler.file));
        }

        // Phase 2: derive package membership from node file paths.
        // Every method/constructor is assigned to `Pkg:<feature>` based on its file.
        let mut members_by_community: BTreeMap<String, Vec<Node>> = BTreeMap::new();
        let mut community_by_member: BTreeMap<String, String> = BTreeMap::new();

        for node in nodes {
            if !matches!(node.kind, NodeKind::Method | NodeKind::Function | NodeKind::Constructor) {
                continue;
            }
            let feat = feature_of(node.id.as_str(), &node.file);
            let pkg_id = format!("Pkg:{}", feat);
            community_by_member.insert(node.id.as_str().to_string(), pkg_id.clone());
            members_by_community.entry(pkg_id).or_default().push(node.clone());
        }

        for members in members_by_community.values_mut() {
            members.sort_by(|a, b| a.name.cmp(&b.name));
        }

        // Phase 3: synthetic community nodes (one per package).
        let mut pkg_names: Vec<String> = members_by_community.keys().cloned().collect();
        pkg_names.sort();

        let community_nodes: Vec<Node> = pkg_names
            .iter()
            .map(|pkg_id| {
                let name = pkg_id.strip_prefix("Pkg:").unwrap_or(pkg_id).to_string();
                Node {
                    id: cih_core::NodeId::new(pkg_id.clone()),
                    kind: NodeKind::Community,
                    name,
                    qualified_name: None,
                    file: String::new(),
                    range: cih_core::Range::default(),
                    props: None,
                }
            })
            .collect();

        // Register synthetic nodes in the id index so community_name() resolves them.
        for n in &community_nodes {
            nodes_by_id.insert(n.id.as_str().to_string(), n.clone());
        }

        let stats = derive_community_stats(
            &members_by_community,
            &community_by_member,
            &idx.calls_out,
            &idx.tests_in,
            &idx.routes,
            &idx.executes_query,
            &idx.query_reads_table,
            &idx.query_writes_table,
            nodes,
            |n| format!("Pkg:{}", feature_of(n.id.as_str(), &n.file)),
        );

        let EdgeIndex {
            calls_out, calls_in, tests_out, tests_in,
            external_calls, publishes, listens, routes,
            methods_by_class, executes_query, query_reads_table,
            query_writes_table, impl_methods,
        } = idx;

        WikiGraph {
            nodes_by_id,
            community_nodes,
            process_nodes: Vec::new(),
            members_by_community,
            community_by_member,
            calls_out,
            calls_in,
            tests_out,
            tests_in,
            external_calls,
            publishes,
            listens,
            routes,
            process_steps: BTreeMap::new(),
            community_routes: stats.community_routes,
            community_tests: stats.community_tests,
            community_class_counts: stats.community_class_counts,
            community_method_counts: stats.community_method_counts,
            community_stereotypes: stats.community_stereotypes,
            inter_community_calls: stats.inter_community_calls,
            methods_by_class,
            impl_methods,
            executes_query,
            query_reads_table,
            query_writes_table,
            community_db_tables: stats.community_db_tables,
            routes_by_controller,
            controller_feature,
        }
    }

    /// Communities that call INTO the given community (callers, with call count).
    pub fn callers_of(&self, community_id: &str) -> Vec<(String, usize)> {
        self.inter_community_calls
            .iter()
            .filter(|(_, dst, _)| dst == community_id)
            .map(|(src, _, cnt)| (src.clone(), *cnt))
            .collect()
    }

    /// Communities that the given community calls INTO (callees, with call count).
    pub fn callees_of(&self, community_id: &str) -> Vec<(String, usize)> {
        self.inter_community_calls
            .iter()
            .filter(|(src, _, _)| src == community_id)
            .map(|(_, dst, cnt)| (dst.clone(), *cnt))
            .collect()
    }

    pub fn community_name<'a>(&'a self, community_id: &'a str) -> &'a str {
        self.nodes_by_id
            .get(community_id)
            .map(|n| n.name.as_str())
            .unwrap_or(community_id)
    }

    /// Returns (publishes, consumes) topic lists for a community.
    /// Each entry is (topic_name, topic_type) e.g. ("OrderCreatedEvent", "kafka").
    pub fn community_messaging(&self, community_id: &str) -> (MessagingPairs, MessagingPairs) {
        let node = match self.nodes_by_id.get(community_id) {
            Some(n) => n,
            None => return (vec![], vec![]),
        };

        // Leiden communities store pre-computed topic lists in props.
        // Package communities (Pkg:xxx) have no props — compute from member edges instead.
        if let Some(props) = &node.props {
            let parse = |key: &str| -> Vec<(String, String)> {
                props
                    .get(key)
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| {
                                let name = item.get("name")?.as_str()?.to_string();
                                let kind = item
                                    .get("type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("event")
                                    .to_string();
                                Some((name, kind))
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            return (parse("publishes_topics"), parse("consumes_topics"));
        }

        // Package mode: derive from publishes/listens edge maps for each member method.
        let members = self
            .members_by_community
            .get(community_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let mut pub_set: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        let mut con_set: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for m in members {
            for topic_id in self.publishes.get(m.id.as_str()).into_iter().flatten() {
                let name = topic_id
                    .strip_prefix("KafkaTopic:")
                    .unwrap_or(topic_id)
                    .to_string();
                pub_set.insert(name, "Kafka".to_string());
            }
            for topic_id in self.listens.get(m.id.as_str()).into_iter().flatten() {
                let name = topic_id
                    .strip_prefix("KafkaTopic:")
                    .unwrap_or(topic_id)
                    .to_string();
                con_set.insert(name, "Kafka".to_string());
            }
        }
        (
            pub_set.into_iter().collect(),
            con_set.into_iter().collect(),
        )
    }

    pub fn community_display_name<'a>(&'a self, community_id: &'a str) -> &'a str {
        self.nodes_by_id
            .get(community_id)
            .and_then(|n| n.props.as_ref())
            .and_then(|p| p.get("display_name"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.community_name(community_id))
    }

    pub fn is_business_process(&self, process_id: &str) -> bool {
        self.nodes_by_id
            .get(process_id)
            .and_then(|n| n.props.as_ref())
            .and_then(|p| p.get("business_flow"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// BFS from `start_id` through `calls_out`, limited to `max_depth` hops.
    /// When the chain hits an interface method, follows `MethodImplements` edges
    /// to the concrete implementation so the chain shows actual code, not stubs.
    pub fn build_call_chain(&self, start_id: &str, max_depth: usize) -> Vec<String> {
        use std::collections::{HashSet, VecDeque};
        let mut visited: HashSet<String> = HashSet::new();
        let mut chain: Vec<String> = Vec::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        queue.push_back((start_id.to_string(), 0));
        while let Some((id, depth)) = queue.pop_front() {
            if visited.contains(&id) || depth > max_depth {
                continue;
            }
            visited.insert(id.clone());
            let cls_id = crate::pages::api_flow::class_id_from_method_id(id.as_str(), self);
            let is_exception_ctor = id.starts_with("Constructor:") && {
                let simple = id
                    .strip_prefix("Constructor:").unwrap_or("")
                    .split('#').next().unwrap_or("")
                    .rsplit('.').next().unwrap_or("");
                simple.ends_with("Exception") || simple.ends_with("Error")
            };
            let is_interface_method = cls_id.starts_with("Interface:");
            if !is_exception_ctor
                && (self.nodes_by_id.contains_key(cls_id.as_str())
                    || self.methods_by_class.contains_key(cls_id.as_str()))
            {
                // Prefer the concrete impl: if this is an interface method that has
                // exactly one known implementation, skip the interface stub and
                // show only the impl. If there are multiple impls, include the
                // interface step so the chain doesn't silently branch.
                let impls = self.impl_methods.get(id.as_str());
                let impl_count = impls.map(|v| v.len()).unwrap_or(0);
                let skip_interface_stub = is_interface_method && impl_count == 1;
                if skip_interface_stub {
                    tracing::debug!(
                        interface_method = %id,
                        impl_method = %impls.unwrap()[0],
                        "interface→impl: skipping interface stub (single impl)"
                    );
                } else if is_interface_method && impl_count > 1 {
                    tracing::debug!(
                        interface_method = %id,
                        impl_count,
                        "interface→impl: keeping stub (multiple impls)"
                    );
                }
                if !skip_interface_stub {
                    chain.push(id.clone());
                }
            }
            // Follow normal call edges
            if let Some(callees) = self.calls_out.get(id.as_str()) {
                for callee in callees {
                    if !visited.contains(callee) {
                        queue.push_back((callee.clone(), depth + 1));
                    }
                }
            }
            // When on an interface method, also follow MethodImplements edges so
            // the concrete impl body (and its callees) appear in the chain.
            if is_interface_method {
                if let Some(impls) = self.impl_methods.get(id.as_str()) {
                    for impl_id in impls {
                        if !visited.contains(impl_id) {
                            tracing::debug!(
                                interface_method = %id,
                                impl_method = %impl_id,
                                depth,
                                "interface→impl: queuing impl"
                            );
                            // Same depth: the impl IS the step, not an extra hop.
                            queue.push_back((impl_id.clone(), depth));
                        }
                    }
                }
            }
        }
        chain
    }

    pub fn processes_for_community(&self, community_id: &str, business_only: bool) -> Vec<String> {
        let mut result = Vec::new();
        for (proc_id, steps) in &self.process_steps {
            if let Some(first) = steps.first() {
                let sym_id = first.symbol.id.as_str().to_string();
                if self.community_by_member.get(&sym_id).map(|c| c.as_str()) == Some(community_id)
                    && (!business_only || self.is_business_process(proc_id)) {
                        result.push(proc_id.clone());
                    }
            }
        }
        result.sort();
        result
    }
}

pub fn node_stereotype(node: &Node) -> Option<&str> {
    node.props
        .as_ref()
        .and_then(|p| p.get("stereotype"))
        .and_then(|v| v.as_str())
}

pub fn route_path(route: &Node) -> String {
    route
        .props
        .as_ref()
        .and_then(|p| p.get("path"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| route.name.split_once(' ').map(|x| x.1).unwrap_or(&route.name))
        .to_string()
}

pub fn route_http_method(route: &Node) -> String {
    route
        .props
        .as_ref()
        .and_then(|p| p.get("httpMethod"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| route.name.split(' ').next().unwrap_or("GET"))
        .to_string()
}

pub fn route_decorator(route: &Node) -> &str {
    let props = route.props.as_ref();
    // Prefer the legacy `decorator` string; fall back to the first entry of the
    // newer `route_annotations` array (Spring MVC + JAX-RS extraction).
    props
        .and_then(|p| p.get("decorator"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            props
                .and_then(|p| p.get("route_annotations"))
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|v| v.as_str())
        })
        .unwrap_or("")
}


