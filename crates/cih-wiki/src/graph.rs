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
    let prefix = "modules/";
    if let Some(start) = file.find(prefix) {
        let rest = &file[start + prefix.len()..];
        if let Some(end) = rest.find('/') {
            if end > 0 {
                return rest[..end].to_string();
            }
        }
    }
    "shared".to_string()
}

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
                        steps_raw
                            .entry(proc_id)
                            .or_default()
                            .push((step_num, sym.clone()));
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

        let mut routes_by_controller: BTreeMap<String, Vec<(Node, Node)>> = BTreeMap::new();
        let mut controller_feature: BTreeMap<String, String> = BTreeMap::new();
        for (handler, route) in &routes {
            let ctrl = controller_name_from_handler_id(handler.id.as_str()).to_string();
            routes_by_controller
                .entry(ctrl.clone())
                .or_default()
                .push((handler.clone(), route.clone()));
            controller_feature
                .entry(ctrl)
                .or_insert_with(|| feature_from_file_path(&handler.file));
        }

        let mut community_routes: BTreeMap<String, Vec<(Node, Node)>> = BTreeMap::new();
        let mut community_tests_set: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut community_class_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut community_method_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut community_stereotypes: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
        let mut cross: BTreeMap<(String, String), usize> = BTreeMap::new();

        for (comm_id, members) in &members_by_community {
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

        for (handler, route) in &routes {
            if let Some(comm_id) = community_by_member.get(handler.id.as_str()) {
                community_routes
                    .entry(comm_id.clone())
                    .or_default()
                    .push((handler.clone(), route.clone()));
            }
        }

        for (comm_id, members) in &members_by_community {
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

        for (src_id, dst_ids) in &calls_out {
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
        for (comm_id, members) in &members_by_community {
            for member in members {
                if let Some(query_ids) = executes_query.get(member.id.as_str()) {
                    for qid in query_ids {
                        for tid in query_reads_table.get(qid.as_str()).into_iter().flatten() {
                            let name = tid.strip_prefix("DbTable:").unwrap_or(tid).to_string();
                            raw_db
                                .entry(comm_id.clone())
                                .or_default()
                                .entry(name)
                                .or_default()
                                .0 = true;
                        }
                        for tid in query_writes_table.get(qid.as_str()).into_iter().flatten() {
                            let name = tid.strip_prefix("DbTable:").unwrap_or(tid).to_string();
                            raw_db
                                .entry(comm_id.clone())
                                .or_default()
                                .entry(name)
                                .or_default()
                                .1 = true;
                        }
                    }
                }
            }
        }
        let community_db_tables: BTreeMap<String, Vec<DbTableAccess>> = raw_db
            .into_iter()
            .map(|(comm_id, tables)| {
                let mut v: Vec<DbTableAccess> = tables
                    .into_iter()
                    .map(|(name, (r, w))| DbTableAccess {
                        table_name: name,
                        reads: r,
                        writes: w,
                    })
                    .collect();
                v.sort_by(|a, b| a.table_name.cmp(&b.table_name));
                (comm_id, v)
            })
            .collect();

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
            community_routes,
            community_tests,
            community_class_counts,
            community_method_counts,
            community_stereotypes,
            inter_community_calls,
            methods_by_class,
            executes_query,
            query_reads_table,
            query_writes_table,
            community_db_tables,
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
    pub fn community_messaging(
        &self,
        community_id: &str,
    ) -> (Vec<(String, String)>, Vec<(String, String)>) {
        let node = match self.nodes_by_id.get(community_id) {
            Some(n) => n,
            None => return (vec![], vec![]),
        };
        let props = match node.props.as_ref() {
            Some(p) => p,
            None => return (vec![], vec![]),
        };
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
        (parse("publishes_topics"), parse("consumes_topics"))
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

    pub fn processes_for_community(&self, community_id: &str, business_only: bool) -> Vec<String> {
        let mut result = Vec::new();
        for (proc_id, steps) in &self.process_steps {
            if let Some(first) = steps.first() {
                let sym_id = first.symbol.id.as_str().to_string();
                if self.community_by_member.get(&sym_id).map(|c| c.as_str()) == Some(community_id) {
                    if !business_only || self.is_business_process(proc_id) {
                        result.push(proc_id.clone());
                    }
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
        .unwrap_or_else(|| route.name.splitn(2, ' ').nth(1).unwrap_or(&route.name))
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

#[cfg(test)]
mod tests;

