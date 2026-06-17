mod bfs;
mod cohesion;
mod entry_points;
pub mod graph;
mod label;
mod leiden;
mod prng;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use cih_core::{community_id, process_id, Edge, EdgeKind, Node, NodeId, NodeKind, Range};
use petgraph::graph::NodeIndex;

pub use graph::{build_calls_digraph, build_community_graph, is_large_graph};

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
            max_iterations: 0,
            seed: 0xc0de,
            large_graph_threshold: 10_001,
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
            max_processes: 20.max(300.min(symbol_count / 10)),
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

    let mut rng = prng::Mulberry32::new(cfg.seed);
    let assignments = leiden::louvain(
        &community_graph,
        cfg.resolution,
        cfg.max_iterations,
        &mut rng,
    );

    let source_by_id: HashMap<&NodeId, &Node> = nodes.iter().map(|n| (&n.id, n)).collect();
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

        out.nodes.push(Node {
            id: comm_id.clone(),
            kind: NodeKind::Community,
            name: label.clone(),
            qualified_name: Some(label.clone()),
            file: String::new(),
            range: Range::default(),
            props: Some(serde_json::json!({
                "label": label,
                "heuristic_label": label,
                "cohesion": cohesion,
                "symbol_count": member_ids.len(),
                "symbolCount": member_ids.len(),
                "color": COLOR_PALETTE[comm_idx % COLOR_PALETTE.len()],
            })),
        });

        for member_id in member_ids {
            out.edges.push(Edge {
                src: member_id.clone(),
                dst: comm_id.clone(),
                kind: EdgeKind::MemberOf,
                confidence: 1.0,
                reason: "leiden".into(),
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
) -> ProcessOutput {
    let (digraph, node_index) = graph::build_calls_digraph(nodes, edges, cfg.min_trace_confidence);
    if digraph.node_count() == 0 {
        return ProcessOutput::default();
    }

    let membership_map: HashMap<NodeId, NodeId> = memberships.iter().cloned().collect();
    let entry_points = entry_points::score_entry_points(nodes, &digraph, &node_index);
    let traces = bfs::trace_process_paths(&digraph, &entry_points, &membership_map, cfg);
    let node_by_id: HashMap<&NodeId, &Node> = nodes.iter().map(|n| (&n.id, n)).collect();

    let mut out = ProcessOutput::default();
    for trace in traces {
        let trace_ids: Vec<NodeId> = trace.iter().map(|idx| digraph[*idx].clone()).collect();
        if trace_ids.len() < cfg.min_steps {
            continue;
        }
        let entry_id = trace_ids
            .first()
            .cloned()
            .unwrap_or_else(|| NodeId::new(""));
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
            })),
        });

        for (step_idx, symbol_id) in trace_ids.into_iter().enumerate() {
            out.edges.push(Edge {
                src: symbol_id,
                dst: proc_id.clone(),
                kind: EdgeKind::StepInProcess,
                confidence: 1.0,
                reason: format!("step:{}", step_idx + 1),
            });
        }
    }

    out
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
mod tests {
    use super::*;
    use cih_core::{method_id, type_id};

    fn class_node(fqcn: &str, file: &str) -> Node {
        Node {
            id: type_id(NodeKind::Class, fqcn),
            kind: NodeKind::Class,
            name: fqcn.rsplit('.').next().unwrap_or(fqcn).to_string(),
            qualified_name: Some(fqcn.to_string()),
            file: file.to_string(),
            range: Range::default(),
            props: None,
        }
    }

    fn method_node(fqcn: &str, name: &str) -> Node {
        Node {
            id: method_id(fqcn, name, 0),
            kind: NodeKind::Method,
            name: name.to_string(),
            qualified_name: Some(format!("{fqcn}#{name}/0")),
            file: format!("src/main/java/{}.java", fqcn.replace('.', "/")),
            range: Range::default(),
            props: None,
        }
    }

    fn edge(src: &NodeId, dst: &NodeId, kind: EdgeKind, confidence: f32) -> Edge {
        Edge {
            src: src.clone(),
            dst: dst.clone(),
            kind,
            confidence,
            reason: String::new(),
        }
    }

    fn call(src: &Node, dst: &Node) -> Edge {
        edge(&src.id, &dst.id, EdgeKind::Calls, 1.0)
    }

    #[test]
    fn community_detection_splits_two_cliques() {
        let nodes = vec![
            class_node("com.acme.a.A1", "src/main/java/com/acme/a/A1.java"),
            class_node("com.acme.a.A2", "src/main/java/com/acme/a/A2.java"),
            class_node("com.acme.a.A3", "src/main/java/com/acme/a/A3.java"),
            class_node("com.acme.b.B1", "src/main/java/com/acme/b/B1.java"),
            class_node("com.acme.b.B2", "src/main/java/com/acme/b/B2.java"),
            class_node("com.acme.b.B3", "src/main/java/com/acme/b/B3.java"),
        ];
        let mut edges = Vec::new();
        for (a, b) in [(0, 1), (1, 2), (0, 2), (3, 4), (4, 5), (3, 5)] {
            edges.push(edge(&nodes[a].id, &nodes[b].id, EdgeKind::Calls, 1.0));
        }
        edges.push(edge(&nodes[2].id, &nodes[3].id, EdgeKind::Calls, 0.05));

        let out = detect_communities(
            &nodes,
            &edges,
            &CommunityConfig {
                max_iterations: 20,
                ..CommunityConfig::default()
            },
        );
        assert_eq!(out.nodes.len(), 2);
        assert_eq!(out.edges.len(), 6);
    }

    #[test]
    fn seeded_rng_is_deterministic() {
        let nodes = vec![
            class_node("com.acme.A", "src/main/java/com/acme/A.java"),
            class_node("com.acme.B", "src/main/java/com/acme/B.java"),
            class_node("com.acme.C", "src/main/java/com/acme/C.java"),
        ];
        let edges = vec![call(&nodes[0], &nodes[1]), call(&nodes[1], &nodes[2])];
        let first = detect_communities(&nodes, &edges, &CommunityConfig::default());
        let second = detect_communities(&nodes, &edges, &CommunityConfig::default());
        let first_edges: Vec<_> = first
            .edges
            .iter()
            .map(|e| (e.src.as_str().to_string(), e.dst.as_str().to_string()))
            .collect();
        let second_edges: Vec<_> = second
            .edges
            .iter()
            .map(|e| (e.src.as_str().to_string(), e.dst.as_str().to_string()))
            .collect();
        assert_eq!(first_edges, second_edges);
    }

    #[test]
    fn singleton_communities_are_discarded() {
        let nodes = vec![class_node(
            "com.acme.Alone",
            "src/main/java/com/acme/Alone.java",
        )];
        let out = detect_communities(&nodes, &[], &CommunityConfig::default());
        assert!(out.nodes.is_empty());
        assert!(out.edges.is_empty());
    }

    #[test]
    fn process_trace_min_steps_enforced() {
        let a = method_node("com.acme.A", "handle");
        let b = method_node("com.acme.B", "work");
        let c = method_node("com.acme.C", "done");

        let short = trace_processes(
            &[a.clone(), b.clone()],
            &[call(&a, &b)],
            &[],
            &ProcessConfig::for_symbol_count(2),
        );
        assert!(short.nodes.is_empty());

        let long = trace_processes(
            &[a.clone(), b.clone(), c.clone()],
            &[call(&a, &b), call(&b, &c)],
            &[],
            &ProcessConfig::for_symbol_count(3),
        );
        assert_eq!(long.nodes.len(), 1);
        assert_eq!(long.edges.len(), 3);
    }

    #[test]
    fn process_cycle_prevention() {
        let a = method_node("com.acme.A", "handle");
        let b = method_node("com.acme.B", "work");
        let c = method_node("com.acme.C", "done");
        let out = trace_processes(
            &[a.clone(), b.clone(), c.clone()],
            &[call(&a, &b), call(&b, &c), call(&c, &a)],
            &[],
            &ProcessConfig::for_symbol_count(3),
        );
        assert!(!out.nodes.is_empty());
        assert!(out.nodes.len() < 10);
    }

    #[test]
    fn process_cross_community() {
        let a = method_node("com.acme.A", "handle");
        let b = method_node("com.acme.B", "work");
        let c = method_node("com.acme.C", "done");
        let memberships = vec![
            (a.id.clone(), community_id(0)),
            (b.id.clone(), community_id(0)),
            (c.id.clone(), community_id(1)),
        ];
        let out = trace_processes(
            &[a.clone(), b.clone(), c.clone()],
            &[call(&a, &b), call(&b, &c)],
            &memberships,
            &ProcessConfig::for_symbol_count(3),
        );
        let process_type = out.nodes[0]
            .props
            .as_ref()
            .and_then(|p| p.get("process_type"))
            .and_then(|v| v.as_str());
        assert_eq!(process_type, Some("cross_community"));
    }

    #[test]
    fn process_dedup_keeps_longest() {
        let a = NodeId::new("Method:A#a/0");
        let b = NodeId::new("Method:B#b/0");
        let c = NodeId::new("Method:C#c/0");
        let d = NodeId::new("Method:D#d/0");
        let mut graph = petgraph::graph::DiGraph::<NodeId, f32>::new();
        let ai = graph.add_node(a);
        let bi = graph.add_node(b);
        let ci = graph.add_node(c);
        let di = graph.add_node(d);
        let deduped =
            crate::bfs::deduplicate_traces(vec![vec![ai, bi, ci], vec![ai, bi, ci, di]], &graph);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0], vec![ai, bi, ci, di]);
    }
}
