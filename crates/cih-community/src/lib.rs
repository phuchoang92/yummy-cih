pub mod bfs;
mod cohesion;
mod entry_points;
pub mod graph;
mod label;
mod leiden;
mod leiden_impl;
pub mod registry;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use cih_core::{community_id, process_id, Edge, EdgeKind, Node, NodeId, NodeKind, Range};
use petgraph::graph::NodeIndex;

pub use cih_core::{
    build_calls_digraph, score_all_entry_points, EntrypointKind, EntrypointRegistry,
    ScoredEntrypoint,
};
pub use graph::{build_community_graph, is_large_graph};

const COLOR_PALETTE: [&str; 12] = [
    "#ef4444", "#f97316", "#eab308", "#22c55e", "#06b6d4", "#3b82f6", "#8b5cf6", "#d946ef",
    "#ec4899", "#f43f5e", "#14b8a6", "#84cc16",
];

#[derive(Clone, Debug)]
pub struct CommunityConfig {
    pub resolution: f64,
    pub max_iterations: u32,
    pub seed: u32,
    pub large_graph_threshold: usize,
    pub min_confidence_large: f32,
    pub min_community_size: usize,
}

impl Default for CommunityConfig {
    fn default() -> Self {
        Self {
            resolution: 1.0,
            max_iterations: 10,
            seed: 0xc0de,
            large_graph_threshold: 10_000,
            min_confidence_large: 0.5,
            min_community_size: 2,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProcessConfig {
    pub max_trace_depth: usize,
    pub max_branching: usize,
    pub max_processes: usize,
    pub min_steps: usize,
    pub min_trace_confidence: f32,
    pub max_states_per_entry: usize,
}

impl ProcessConfig {
    pub fn for_symbol_count(symbol_count: usize) -> Self {
        Self {
            max_trace_depth: 10,
            max_branching: 4,
            max_processes: (symbol_count / 10).clamp(5, 300),
            min_steps: 3,
            min_trace_confidence: 0.5,
            max_states_per_entry: 50_000,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CommunityOutput {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub memberships: Vec<(NodeId, NodeId)>,
}

#[derive(Clone, Debug, Default)]
pub struct ProcessOutput {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

pub fn detect_communities(
    nodes: &[Node],
    edges: &[Edge],
    cfg: &CommunityConfig,
) -> CommunityOutput {
    let large = graph::symbol_node_count(nodes) > cfg.large_graph_threshold;
    let (community_graph, _) =
        graph::build_community_graph(nodes, edges, large, cfg.min_confidence_large);
    if community_graph.node_count() == 0 {
        return CommunityOutput::default();
    }

    let assignments = leiden::leiden(
        &community_graph,
        cfg.resolution,
        cfg.max_iterations as usize,
        cfg.seed as u64,
    );

    let source_by_id: HashMap<&NodeId, &Node> = nodes.iter().map(|n| (&n.id, n)).collect();

    // Edge lookups for community enrichment
    let mut route_nodes_by_handler: HashMap<NodeId, Vec<&Node>> = HashMap::new();
    let mut queries_by_method: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut read_tables_by_query: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut write_tables_by_query: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    // publishes_by_member and consumes_by_member store (topic_node, topic_kind_str)
    let mut publishes_by_member: HashMap<NodeId, Vec<(&Node, &'static str)>> = HashMap::new();
    let mut consumes_by_member: HashMap<NodeId, Vec<(&Node, &'static str)>> = HashMap::new();
    for e in edges {
        match e.kind {
            EdgeKind::HandlesRoute => {
                if let Some(rn) = source_by_id.get(&e.dst) {
                    route_nodes_by_handler
                        .entry(e.src.clone())
                        .or_default()
                        .push(rn);
                }
            }
            EdgeKind::ExecutesQuery => {
                queries_by_method
                    .entry(e.src.clone())
                    .or_default()
                    .push(e.dst.clone());
            }
            EdgeKind::ReadsTable => {
                read_tables_by_query
                    .entry(e.src.clone())
                    .or_default()
                    .push(e.dst.clone());
            }
            EdgeKind::WritesTable => {
                write_tables_by_query
                    .entry(e.src.clone())
                    .or_default()
                    .push(e.dst.clone());
            }
            EdgeKind::PublishesEvent => {
                if let Some(tn) = source_by_id.get(&e.dst) {
                    let kind_str = topic_kind_str(tn);
                    publishes_by_member
                        .entry(e.src.clone())
                        .or_default()
                        .push((tn, kind_str));
                }
            }
            EdgeKind::ListensTo => {
                if let Some(tn) = source_by_id.get(&e.dst) {
                    let kind_str = topic_kind_str(tn);
                    consumes_by_member
                        .entry(e.src.clone())
                        .or_default()
                        .push((tn, kind_str));
                }
            }
            _ => {}
        }
    }

    let mut groups: BTreeMap<usize, Vec<NodeIndex>> = BTreeMap::new();
    for idx in community_graph.node_indices() {
        let Some(comm) = assignments.get(idx.index()).copied() else {
            continue;
        };
        groups.entry(comm).or_default().push(idx);
    }

    let mut groups: Vec<Vec<NodeIndex>> = groups
        .into_values()
        .filter(|members| members.len() >= cfg.min_community_size)
        .collect();
    groups.sort_by(|a, b| {
        b.len()
            .cmp(&a.len())
            .then_with(|| smallest_id(a, &community_graph).cmp(&smallest_id(b, &community_graph)))
    });

    let mut out = CommunityOutput::default();
    for (comm_idx, members) in groups.iter().enumerate() {
        let comm_id = community_id(comm_idx);
        let mut member_ids: Vec<NodeId> = members
            .iter()
            .map(|idx| community_graph[*idx].clone())
            .collect();
        member_ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));

        let member_files: Vec<&str> = member_ids
            .iter()
            .filter_map(|id| source_by_id.get(id).map(|n| n.file.as_str()))
            .filter(|file| !file.is_empty())
            .collect();
        let label = label::heuristic_label(&member_files, comm_idx);
        let cohesion = cohesion::cohesion_score(members, &community_graph, 50);

        // --- semantic enrichment ---
        // Gather route prefixes
        let mut route_prefix_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut all_route_prefixes: BTreeSet<String> = BTreeSet::new();
        for mid in &member_ids {
            for rn in route_nodes_by_handler.get(mid).into_iter().flatten() {
                let path = rn
                    .props
                    .as_ref()
                    .and_then(|p| p.get("path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| rn.name.splitn(2, ' ').nth(1).unwrap_or(&rn.name));
                if let Some(seg) = first_non_generic_path_segment(path) {
                    *route_prefix_counts.entry(seg.clone()).or_insert(0) += 1;
                    all_route_prefixes.insert(seg);
                }
            }
        }

        // Gather controller class names
        let mut controller_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut all_controllers: BTreeSet<String> = BTreeSet::new();
        for mid in &member_ids {
            let id_str = mid.as_str();
            let without_kind = id_str
                .strip_prefix("Method:")
                .or_else(|| id_str.strip_prefix("Constructor:"))
                .or_else(|| id_str.strip_prefix("Function:"))
                .unwrap_or(id_str);
            if let Some(fqcn) = without_kind.split('#').next() {
                if let Some(simple) = fqcn.rsplit('.').next() {
                    if simple.ends_with("Controller") || simple.ends_with("Resource") {
                        let name = simple
                            .strip_suffix("Controller")
                            .or_else(|| simple.strip_suffix("Resource"))
                            .unwrap_or(simple);
                        // strip role prefix (Admin/Pos/Public) to get domain e.g. "ActivityLog"
                        let domain = strip_role_prefix(name).to_string();
                        *controller_counts.entry(domain.clone()).or_insert(0) += 1;
                        all_controllers.insert(simple.to_string());
                    }
                }
            }
        }

        // Gather DB table names
        let mut table_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut all_tables: BTreeSet<String> = BTreeSet::new();
        for mid in &member_ids {
            for qid in queries_by_method.get(mid).into_iter().flatten() {
                for tid in read_tables_by_query
                    .get(qid)
                    .into_iter()
                    .flatten()
                    .chain(write_tables_by_query.get(qid).into_iter().flatten())
                {
                    let tname = tid
                        .as_str()
                        .strip_prefix("DbTable:")
                        .unwrap_or(tid.as_str());
                    all_tables.insert(tname.to_string());
                    *table_counts.entry(tname.to_string()).or_insert(0) += 1;
                }
            }
        }

        // Gather topic names: separate publish vs consume; infer type from node kind
        let mut topic_domain_counts: BTreeMap<String, usize> = BTreeMap::new();
        // (name, type_str) sorted sets for props
        let mut all_publishes: BTreeSet<(String, String)> = BTreeSet::new();
        let mut all_consumes: BTreeSet<(String, String)> = BTreeSet::new();
        for mid in &member_ids {
            for (tn, kind_str) in publishes_by_member.get(mid).into_iter().flatten() {
                let name = topic_display_name(tn);
                all_publishes.insert((name.clone(), kind_str.to_string()));
                let domain = strip_event_suffix(strip_role_prefix(&name));
                *topic_domain_counts.entry(domain.to_string()).or_insert(0) += 1;
            }
            for (tn, kind_str) in consumes_by_member.get(mid).into_iter().flatten() {
                let name = topic_display_name(tn);
                all_consumes.insert((name.clone(), kind_str.to_string()));
                let domain = strip_event_suffix(strip_role_prefix(&name));
                *topic_domain_counts.entry(domain.to_string()).or_insert(0) += 1;
            }
        }
        // flat names list for the legacy topics prop
        let all_topics: BTreeSet<String> = all_publishes
            .iter()
            .chain(all_consumes.iter())
            .map(|(n, _)| n.clone())
            .collect();

        // Apply naming waterfall
        let (display_name, naming_reason) = 'naming: {
            // 1. Route prefix
            if let Some(seg) = best_by_count(&route_prefix_counts) {
                let n = capitalize_first(&seg);
                break 'naming (n, "route_prefix");
            }
            // 2. Controller — seg is PascalCase domain e.g. "ActivityLog"
            if let Some(seg) = best_by_count(&controller_counts) {
                break 'naming (seg, "controller");
            }
            // 3. DB table prefix
            if let Some(tname) = best_by_count(&table_counts) {
                if let Some(tok) = first_non_generic_name_token(&tname, &['_', '.', '-', '/']) {
                    let n = capitalize_first(&tok);
                    break 'naming (n, "db_table");
                }
            }
            // 4. Topic — domain is already PascalCase e.g. "LowStock", "OrderCancelled"
            if let Some(domain) = best_by_count(&topic_domain_counts) {
                if !domain.is_empty() {
                    break 'naming (domain, "topic");
                }
            }
            // 5. Folder heuristic
            if !label.is_empty() && label != format!("Cluster_{}", comm_idx) {
                break 'naming (label.clone(), "folder");
            }
            // 6. Fallback
            (format!("Cluster_{}", comm_idx), "fallback")
        };

        let feature = if naming_reason == "fallback" {
            String::new()
        } else {
            pascal_to_kebab_slug(&display_name)
        };

        let primary_stereotype = {
            let mut stereo_counts: BTreeMap<&str, usize> = BTreeMap::new();
            for mid in &member_ids {
                // stereotypes are on Class nodes; derive class id from method id
                // Method:pkg.ClassName#method/arity → Class:pkg.ClassName
                let class_id = mid.as_str().split_once('#').map(|(prefix, _)| {
                    let fqcn = prefix
                        .trim_start_matches("Method:")
                        .trim_start_matches("Constructor:")
                        .trim_start_matches("Function:");
                    format!("Class:{}", fqcn)
                });
                let class_node_id = class_id.as_ref().map(|s| NodeId::new(s.clone()));
                let node = class_node_id
                    .as_ref()
                    .and_then(|cid| source_by_id.get(cid))
                    .or_else(|| source_by_id.get(mid));
                if let Some(n) = node {
                    if let Some(s) = n
                        .props
                        .as_ref()
                        .and_then(|p| p.get("stereotype"))
                        .and_then(|v| v.as_str())
                    {
                        *stereo_counts.entry(s).or_insert(0) += 1;
                    }
                }
            }
            stereo_counts
                .into_iter()
                .max_by(|(a_k, a_v), (b_k, b_v)| a_v.cmp(b_v).then(b_k.cmp(a_k).reverse()))
                .map(|(k, _)| k.to_string())
        };

        let route_prefixes_sorted: Vec<String> = all_route_prefixes.into_iter().collect();
        let controllers_sorted: Vec<String> = all_controllers.into_iter().collect();
        let tables_sorted: Vec<String> = all_tables.into_iter().collect();
        let topics_sorted: Vec<String> = all_topics.into_iter().collect();
        // [{"name": "OrderCreatedEvent", "type": "kafka"}, ...]
        let publishes_topics: Vec<serde_json::Value> = all_publishes
            .into_iter()
            .map(|(n, t)| serde_json::json!({"name": n, "type": t}))
            .collect();
        let consumes_topics: Vec<serde_json::Value> = all_consumes
            .into_iter()
            .map(|(n, t)| serde_json::json!({"name": n, "type": t}))
            .collect();

        out.nodes.push(Node {
            id: comm_id.clone(),
            kind: NodeKind::Community,
            name: display_name.clone(),
            qualified_name: Some(display_name.clone()),
            file: String::new(),
            range: Range::default(),
            props: Some(serde_json::json!({
                "label": label,
                "heuristic_label": label,
                "cohesion": cohesion,
                "symbol_count": member_ids.len(),
                "symbolCount": member_ids.len(),
                "color": COLOR_PALETTE[comm_idx % COLOR_PALETTE.len()],
                "display_name": display_name,
                "feature": feature,
                "naming_reason": naming_reason,
                "route_prefixes": route_prefixes_sorted,
                "controllers": controllers_sorted,
                "db_tables": tables_sorted,
                "topics": topics_sorted,
                "publishes_topics": publishes_topics,
                "consumes_topics": consumes_topics,
                "primary_stereotype": primary_stereotype,
            })),
        });

        for member_id in member_ids {
            out.edges.push(Edge {
                src: member_id.clone(),
                dst: comm_id.clone(),
                kind: EdgeKind::MemberOf,
                confidence: 1.0,
                reason: "leiden".into(),
            props: None,
            });
            out.memberships.push((member_id, comm_id.clone()));
        }
    }
    out
}

pub fn trace_processes(
    nodes: &[Node],
    edges: &[Edge],
    memberships: &[(NodeId, NodeId)],
    cfg: &ProcessConfig,
    registry: &EntrypointRegistry,
) -> ProcessOutput {
    let (digraph, node_index) = cih_core::build_calls_digraph(nodes, edges, cfg.min_trace_confidence);
    if digraph.node_count() == 0 {
        return ProcessOutput::default();
    }

    let membership_map: HashMap<NodeId, NodeId> = memberships.iter().cloned().collect();
    let scored = cih_core::score_entry_points(nodes, edges, &digraph, &node_index, registry);
    let legacy_pairs = cih_core::to_legacy_pairs(&scored);
    let ep_by_id: HashMap<NodeId, &cih_core::ScoredEntrypoint> =
        scored.iter().map(|s| (s.id.clone(), s)).collect();
    let traces = bfs::trace_process_paths(&digraph, &legacy_pairs, &membership_map, cfg);
    let node_by_id: HashMap<&NodeId, &Node> = nodes.iter().map(|n| (&n.id, n)).collect();

    // Track which semantic entrypoints produced at least one accepted trace
    let mut covered_entries: HashSet<String> = HashSet::new();

    let mut out = ProcessOutput::default();
    for trace in traces {
        let trace_ids: Vec<NodeId> = trace.iter().map(|idx| digraph[*idx].clone()).collect();
        let entry = trace_ids
            .first()
            .cloned()
            .unwrap_or_else(|| NodeId::new(""));
        // HttpRoute / EventListener bypass min_steps; others must meet it
        let is_semantic = ep_by_id
            .get(&entry)
            .map(|ep| ep.kind.business_flow())
            .unwrap_or(false);
        if !is_semantic && trace_ids.len() < cfg.min_steps {
            continue;
        }
        covered_entries.insert(entry.as_str().to_string());
        let entry_id = entry.clone();
        let terminal_id = trace_ids.last().cloned().unwrap_or_else(|| NodeId::new(""));
        let entry_name = display_name(&entry_id, &node_by_id);
        let terminal_name = display_name(&terminal_id, &node_by_id);
        let label = format!("{entry_name} -> {terminal_name}");
        let trace_key = trace_ids
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>()
            .join("->");
        let trace_hash = blake3::hash(trace_key.as_bytes()).to_hex()[..6].to_string();
        let proc_id = process_id(&slugify(&entry_name), &trace_hash);
        let communities: Vec<String> = trace_ids
            .iter()
            .filter_map(|id| membership_map.get(id))
            .map(|id| id.as_str().to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let cross_community = communities.len() > 1;

        let (
            ek_str,
            business_flow,
            business_surface,
            route_path_val,
            route_method_val,
            event_topics_val,
        ) = if let Some(ep) = ep_by_id.get(&entry_id) {
            (
                ep.kind.as_str(),
                ep.kind.business_flow(),
                ep.kind.business_surface(),
                ep.route_path.clone(),
                ep.route_method.clone(),
                ep.event_topics.clone(),
            )
        } else {
            ("fanout", false, "internal", None, None, Vec::new())
        };

        out.nodes.push(Node {
            id: proc_id.clone(),
            kind: NodeKind::Process,
            name: label.clone(),
            qualified_name: Some(label.clone()),
            file: String::new(),
            range: Range::default(),
            props: Some(serde_json::json!({
                "label": label,
                "process_type": if cross_community { "cross_community" } else { "intra_community" },
                "step_count": trace_ids.len(),
                "communities": communities,
                "entry_point_id": entry_id.as_str(),
                "terminal_id": terminal_id.as_str(),
                "entrypoint_kind": ek_str,
                "business_flow": business_flow,
                "business_surface": business_surface,
                "route_path": route_path_val,
                "route_method": route_method_val,
                "event_topics": event_topics_val,
            })),
        });

        for (step_idx, symbol_id) in trace_ids.into_iter().enumerate() {
            out.edges.push(Edge {
                src: symbol_id,
                dst: proc_id.clone(),
                kind: EdgeKind::StepInProcess,
                confidence: 1.0,
                reason: format!("step:{}", step_idx + 1),
            props: None,
            });
        }
    }

    // Shallow one-step processes for semantic entries that produced no accepted trace
    for ep in scored.iter().filter(|ep| ep.kind.business_flow()) {
        if covered_entries.contains(ep.id.as_str()) {
            continue;
        }
        let entry_name = display_name(&ep.id, &node_by_id);
        let trace_hash = blake3::hash(ep.id.as_str().as_bytes()).to_hex()[..6].to_string();
        let proc_id = process_id(&slugify(&entry_name), &trace_hash);
        let communities: Vec<String> = membership_map
            .get(&ep.id)
            .map(|cid| vec![cid.as_str().to_string()])
            .unwrap_or_default();
        out.nodes.push(Node {
            id: proc_id.clone(),
            kind: NodeKind::Process,
            name: entry_name.clone(),
            qualified_name: Some(entry_name.clone()),
            file: String::new(),
            range: Range::default(),
            props: Some(serde_json::json!({
                "label": entry_name,
                "process_type": "intra_community",
                "step_count": 1,
                "communities": communities,
                "entry_point_id": ep.id.as_str(),
                "terminal_id": ep.id.as_str(),
                "entrypoint_kind": ep.kind.as_str(),
                "business_flow": ep.kind.business_flow(),
                "business_surface": ep.kind.business_surface(),
                "route_path": ep.route_path,
                "route_method": ep.route_method,
                "event_topics": ep.event_topics,
            })),
        });
        out.edges.push(Edge {
            src: ep.id.clone(),
            dst: proc_id,
            kind: EdgeKind::StepInProcess,
            confidence: 1.0,
            reason: "step:1".into(),
            props: None,
        });
    }

    out
}

fn topic_kind_str(node: &Node) -> &'static str {
    match node.kind {
        NodeKind::KafkaTopic => "kafka",
        _ => {
            if node.id.as_str().starts_with("KafkaTopic:") {
                "kafka"
            } else {
                "event"
            }
        }
    }
}

fn topic_display_name(node: &Node) -> String {
    if !node.name.is_empty() {
        node.name.clone()
    } else {
        node.id
            .as_str()
            .strip_prefix("KafkaTopic:")
            .unwrap_or(node.id.as_str())
            .to_string()
    }
}

/// Strip a trailing infrastructure suffix from a PascalCase name.
/// "OrderStatusChangedEvent" → "OrderStatusChanged", "LowStockMessage" → "LowStock"
fn strip_event_suffix(name: &str) -> &str {
    const SUFFIXES: &[&str] = &[
        "Event",
        "Events",
        "Message",
        "Messages",
        "Notification",
        "Notifications",
        "Command",
        "Commands",
    ];
    for suffix in SUFFIXES {
        if let Some(rest) = name.strip_suffix(suffix) {
            if !rest.is_empty() {
                return rest;
            }
        }
    }
    name
}

/// Strip a leading role prefix word from a PascalCase name.
/// "AdminActivityLog" → "ActivityLog", "PosOrder" → "Order", "ActivityLog" → "ActivityLog"
///
/// The prefix list is intentionally domain-specific (POS/retail/multi-role app conventions).
/// Move it into [`CommunityConfig`] if the calling domain changes.
fn strip_role_prefix(name: &str) -> &str {
    const PREFIXES: &[&str] = &["Admin", "Pos", "Public", "Private", "Internal"];
    for prefix in PREFIXES {
        if let Some(rest) = name.strip_prefix(prefix) {
            // Only strip if what follows starts with an uppercase letter (proper word boundary)
            if rest
                .chars()
                .next()
                .map(|c| c.is_ascii_uppercase())
                .unwrap_or(false)
            {
                return rest;
            }
        }
    }
    name
}

/// Convert a PascalCase name to a kebab-case feature slug.
/// "ActivityLog" → "activity-log", "AdminBanner" → "admin-banner", "Orders" → "orders"
fn pascal_to_kebab_slug(name: &str) -> String {
    let mut out = String::new();
    for (i, ch) in name.char_indices() {
        if ch.is_ascii_uppercase() && i > 0 {
            out.push('-');
        }
        out.push(ch.to_ascii_lowercase());
    }
    if out.is_empty() {
        "shared".to_string()
    } else {
        out
    }
}

fn first_non_generic_path_segment(path: &str) -> Option<String> {
    const GENERIC: &[&str] = &[
        "api", "apis", "rest", "internal", "external", "service", "services", "common", "shared",
        "core", "app", "apps", "admin", "pos", "public", "private",
    ];
    for part in path.split('/') {
        let part = part.trim();
        if part.is_empty() || part.starts_with('{') || part.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let mut chars = part.chars();
        if matches!(chars.next(), Some('v') | Some('V')) && chars.all(|c| c.is_ascii_digit()) {
            continue;
        }
        let lower = part.to_lowercase();
        if GENERIC.contains(&lower.as_str()) {
            continue;
        }
        return Some(lower);
    }
    None
}

fn first_non_generic_name_token(name: &str, seps: &[char]) -> Option<String> {
    const GENERIC: &[&str] = &[
        "api", "apis", "rest", "internal", "external", "service", "services", "common", "shared",
        "core", "app", "apps",
    ];
    for part in name.split(seps) {
        let part = part.trim();
        if part.is_empty() || part.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let lower = part.to_lowercase();
        if GENERIC.contains(&lower.as_str()) {
            continue;
        }
        return Some(lower);
    }
    None
}

fn best_by_count(counts: &BTreeMap<String, usize>) -> Option<String> {
    counts
        .iter()
        .max_by(|(a_k, a_v), (b_k, b_v)| a_v.cmp(b_v).then(b_k.cmp(a_k).reverse()))
        .map(|(k, _)| k.clone())
}

fn capitalize_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().to_string() + c.as_str(),
    }
}

fn smallest_id(members: &[NodeIndex], graph: &petgraph::graph::UnGraph<NodeId, f32>) -> String {
    members
        .iter()
        .map(|idx| graph[*idx].as_str())
        .min()
        .unwrap_or("")
        .to_string()
}

fn display_name(id: &NodeId, node_by_id: &HashMap<&NodeId, &Node>) -> String {
    node_by_id
        .get(id)
        .map(|n| {
            if n.name.is_empty() {
                id.as_str().to_string()
            } else {
                n.name.clone()
            }
        })
        .unwrap_or_else(|| id.as_str().to_string())
}

fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.to_ascii_lowercase().chars() {
        match ch {
            ':' | '#' | '/' => out.push('-'),
            c if c.is_ascii_alphanumeric() || c == '-' || c == '_' => out.push(c),
            _ => {}
        }
    }
    if out.is_empty() {
        "process".to_string()
    } else {
        out
    }
}


#[cfg(test)]
mod tests;

