use crate::graph::WikiGraph;

const MAX_NODES: usize = 20;
const MAX_EDGES: usize = 30;

/// Escape a label for use inside a Mermaid `["..."]` node.
fn sanitize(s: &str) -> String {
    s.replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\n', " ")
        // double-dash is parsed as an arrow; replace with em-dash
        .replace("--", "—")
}

fn node_id(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

/// Generate a `flowchart LR` process-step diagram for a feature's communities.
/// Returns `None` if there are fewer than 2 connected steps.
pub fn process_flow_diagram(graph: &WikiGraph, community_ids: &[String]) -> Option<String> {
    let mut steps: Vec<(String, String)> = Vec::new(); // (id, label)
    let mut arrows: Vec<(String, String)> = Vec::new(); // (from_id, to_id)

    for comm_id in community_ids {
        let Some(members) = graph.members_by_community.get(comm_id) else {
            continue;
        };
        for member in members {
            let mid = member.id.as_str();
            if let Some(proc_list) = find_processes_for_member(graph, mid) {
                for proc_id in proc_list {
                    if let Some(proc_steps) = graph.process_steps.get(proc_id.as_str()) {
                        for pair in proc_steps.windows(2) {
                            let from_label = sanitize(&pair[0].symbol.name);
                            let to_label = sanitize(&pair[1].symbol.name);
                            let from_nid = node_id(&pair[0].symbol.id.as_str().replace("Method:", ""));
                            let to_nid = node_id(&pair[1].symbol.id.as_str().replace("Method:", ""));
                            if !steps.iter().any(|(id, _)| id == &from_nid) {
                                steps.push((from_nid.clone(), from_label));
                            }
                            if !steps.iter().any(|(id, _)| id == &to_nid) {
                                steps.push((to_nid.clone(), to_label));
                            }
                            if !arrows.contains(&(from_nid.clone(), to_nid.clone())) {
                                arrows.push((from_nid, to_nid));
                            }
                            if steps.len() >= MAX_NODES || arrows.len() >= MAX_EDGES {
                                break;
                            }
                        }
                    }
                    if steps.len() >= MAX_NODES {
                        break;
                    }
                }
            }
        }
    }

    if steps.len() < 2 || arrows.is_empty() {
        return None;
    }

    let truncated = steps.len() >= MAX_NODES || arrows.len() >= MAX_EDGES;
    let mut out = String::from("flowchart LR\n");
    for (id, label) in &steps {
        out.push_str(&format!("  {}[\"{}\"]\n", id, label));
    }
    for (from, to) in &arrows {
        out.push_str(&format!("  {} --> {}\n", from, to));
    }
    if truncated {
        out.push_str("  %%diagram truncated\n");
    }
    Some(out)
}

fn find_processes_for_member<'a>(
    graph: &'a WikiGraph,
    member_id: &str,
) -> Option<Vec<&'a String>> {
    let result: Vec<&'a String> = graph
        .process_steps
        .iter()
        .filter_map(|(proc_id, steps)| {
            if steps.iter().any(|s| s.symbol.id.as_str() == member_id) {
                Some(proc_id)
            } else {
                None
            }
        })
        .collect();
    if result.is_empty() { None } else { Some(result) }
}

/// Generate a `flowchart LR` diagram showing how communities call each other,
/// filtered to calls involving `comm_id`.
pub fn community_call_diagram(graph: &WikiGraph, comm_id: &str) -> Option<String> {
    let relevant: Vec<&(String, String, usize)> = graph
        .inter_community_calls
        .iter()
        .filter(|(src, dst, _)| src == comm_id || dst == comm_id)
        .collect();

    if relevant.len() < 1 {
        return None;
    }

    // Collect unique community IDs involved
    let mut comm_ids: Vec<&str> = Vec::new();
    for (src, dst, _) in &relevant {
        if !comm_ids.contains(&src.as_str()) {
            comm_ids.push(src);
        }
        if !comm_ids.contains(&dst.as_str()) {
            comm_ids.push(dst);
        }
        if comm_ids.len() >= MAX_NODES {
            break;
        }
    }

    if comm_ids.len() < 2 {
        return None;
    }

    let truncated = comm_ids.len() >= MAX_NODES || relevant.len() >= MAX_EDGES;
    let mut out = String::from("flowchart LR\n");
    for cid in &comm_ids {
        let label = sanitize(&graph.community_name(cid));
        let nid = node_id(cid);
        out.push_str(&format!("  {}[\"{}\"]\n", nid, label));
    }
    for (src, dst, count) in relevant.iter().take(MAX_EDGES) {
        let label = if *count > 1 {
            format!(" --\"{}x\"--> ", count)
        } else {
            " --> ".to_string()
        };
        out.push_str(&format!("  {}{}{}\n", node_id(src), label, node_id(dst)));
    }
    if truncated {
        out.push_str("  %%diagram truncated\n");
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};

    fn simple_node(id: &str, kind: NodeKind, name: &str) -> Node {
        Node {
            id: NodeId::new(id.to_string()),
            kind,
            name: name.to_string(),
            qualified_name: None,
            file: String::new(),
            range: Range::default(),
            props: None,
        }
    }

    #[test]
    fn sanitize_escapes_quotes_and_dashes() {
        assert!(!sanitize("Hello \"world\"").contains('"'));
        assert!(!sanitize("a--b").contains("--"));
        assert!(!sanitize("<type>").contains('<'));
    }

    #[test]
    fn community_call_diagram_requires_at_least_two_communities() {
        let g = WikiGraph::build(&[], &[], &[], &[]);
        assert!(community_call_diagram(&g, "Community:0").is_none());
    }

    #[test]
    fn community_call_diagram_produces_flowchart() {
        let c0 = simple_node("Community:0", NodeKind::Community, "order");
        let c1 = simple_node("Community:1", NodeKind::Community, "payment");
        let m0 = simple_node("Method:a#f/0", NodeKind::Method, "f");
        let m1 = simple_node("Method:b#g/0", NodeKind::Method, "g");
        let edges = [
            Edge {
                src: m0.id.clone(),
                dst: c0.id.clone(),
                kind: EdgeKind::MemberOf,
                confidence: 1.0,
                reason: String::new(),
            },
            Edge {
                src: m1.id.clone(),
                dst: c1.id.clone(),
                kind: EdgeKind::MemberOf,
                confidence: 1.0,
                reason: String::new(),
            },
            Edge {
                src: m0.id.clone(),
                dst: m1.id.clone(),
                kind: EdgeKind::Calls,
                confidence: 1.0,
                reason: String::new(),
            },
        ];
        let g = WikiGraph::build(&[m0, m1], &edges[2..], &[c0, c1], &edges[..2]);
        let result = community_call_diagram(&g, "Community:0");
        assert!(result.is_some());
        let diagram = result.unwrap();
        assert!(diagram.starts_with("flowchart LR"));
        assert!(diagram.contains("order") || diagram.contains("Community_0"));
    }

    #[test]
    fn process_flow_diagram_returns_none_for_empty_graph() {
        let g = WikiGraph::build(&[], &[], &[], &[]);
        assert!(process_flow_diagram(&g, &[]).is_none());
    }
}
