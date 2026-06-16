use std::collections::BTreeMap;
use cih_core::{Node, NodeKind};
use crate::graph::{route_http_method, route_path, WikiGraph};
use crate::CommunityLlmSummary;

pub fn render_ba_index(graph: &WikiGraph) -> String {
    let mut md = String::new();
    md.push_str("---\nid: ba/index\ntitle: Workflow Overview\n---\n\n");
    md.push_str("# Workflow Overview\n\n");

    md.push_str(&format!(
        "**Communities:** {} · **Processes:** {} · **Routes:** {}\n\n",
        graph.community_nodes.len(),
        graph.process_nodes.len(),
        graph.routes.len(),
    ));

    if !graph.process_nodes.is_empty() {
        md.push_str("## Execution Chains\n\n");
        for proc in &graph.process_nodes {
            md.push_str(&format!("- **{}**", proc.name));
            if let Some(steps) = graph.process_steps.get(proc.id.as_str()) {
                let chain: Vec<&str> = steps.iter().map(|s| s.symbol.name.as_str()).collect();
                if !chain.is_empty() {
                    md.push_str(&format!(": {}", chain.join(" → ")));
                }
            }
            md.push('\n');
        }
        md.push('\n');
    }

    if !graph.inter_community_calls.is_empty() {
        md.push_str("## Cross-Module Dependencies\n\n");
        md.push_str("| Caller | Callee | Calls |\n");
        md.push_str("|---|---|---|\n");
        for (src, dst, count) in &graph.inter_community_calls {
            let src_name = graph.community_name(src);
            let dst_name = graph.community_name(dst);
            md.push_str(&format!("| {} | {} | {} |\n", src_name, dst_name, count));
        }
        md.push('\n');
    }

    let all_topics: Vec<&Node> = graph
        .nodes_by_id
        .values()
        .filter(|n| n.kind == NodeKind::KafkaTopic)
        .collect();
    if !all_topics.is_empty() {
        md.push_str("## Event Contracts\n\n");
        md.push_str("| Topic | Published by | Subscribed by |\n");
        md.push_str("|---|---|---|\n");
        for topic in all_topics {
            let topic_id = topic.id.as_str().to_string();
            let pub_names: Vec<&str> = graph
                .publishes
                .iter()
                .filter(|(_, v)| v.contains(&topic_id))
                .filter_map(|(k, _)| graph.nodes_by_id.get(k).map(|n| n.name.as_str()))
                .collect();
            let sub_names: Vec<&str> = graph
                .listens
                .iter()
                .filter(|(_, v)| v.contains(&topic_id))
                .filter_map(|(k, _)| graph.nodes_by_id.get(k).map(|n| n.name.as_str()))
                .collect();
            md.push_str(&format!(
                "| `{}` | {} | {} |\n",
                topic.name,
                if pub_names.is_empty() { "—".to_string() } else { pub_names.join(", ") },
                if sub_names.is_empty() { "—".to_string() } else { sub_names.join(", ") },
            ));
        }
        md.push('\n');
    }

    md
}

pub fn render_ba_community(
    graph: &WikiGraph,
    community: &Node,
    slug_map: &BTreeMap<String, String>,
    llm: Option<&CommunityLlmSummary>,
) -> String {
    let comm_id = community.id.as_str();
    let slug = slug_map.get(comm_id).map(|s| s.as_str()).unwrap_or(comm_id);

    let mut md = String::new();
    md.push_str(&format!(
        "---\nid: ba/{}\ntitle: {}\n---\n\n",
        slug, community.name
    ));
    md.push_str(&format!("# {} — Workflow\n\n", community.name));

    if let Some(summary) = llm {
        if !summary.ba.is_empty() {
            md.push_str("## Workflow Summary\n\n");
            md.push_str(&summary.ba);
            md.push_str("\n\n");
        }
    }

    let procs = processes_for_community(graph, comm_id);
    if !procs.is_empty() {
        md.push_str("## Workflows\n\n");
        for proc_id in &procs {
            if let Some(proc_node) = graph.nodes_by_id.get(proc_id) {
                md.push_str(&format!("### {}\n\n", proc_node.name));
                if let Some(steps) = graph.process_steps.get(proc_id.as_str()) {
                    for (i, step) in steps.iter().enumerate() {
                        md.push_str(&format!("{}. {}\n", i + 1, step.symbol.name));
                    }
                    md.push('\n');
                }
            }
        }
    }

    let callers = graph.callers_of(comm_id);
    if !callers.is_empty() {
        md.push_str("## Consumed By\n\n");
        for (caller_id, count) in callers {
            let name = graph.community_name(&caller_id);
            md.push_str(&format!("- {} ({} calls)\n", name, count));
        }
        md.push('\n');
    }

    let callees = graph.callees_of(comm_id);
    if !callees.is_empty() {
        md.push_str("## Consumes\n\n");
        for (callee_id, count) in callees {
            let name = graph.community_name(&callee_id);
            md.push_str(&format!("- {} ({} calls)\n", name, count));
        }
        md.push('\n');
    }

    let empty_members: Vec<Node> = Vec::new();
    let member_list = graph
        .members_by_community
        .get(comm_id)
        .unwrap_or(&empty_members);

    let mut published_topics: Vec<String> = Vec::new();
    let mut subscribed_topics: Vec<String> = Vec::new();

    for m in member_list {
        if let Some(topic_ids) = graph.publishes.get(m.id.as_str()) {
            for tid in topic_ids {
                if let Some(t) = graph.nodes_by_id.get(tid) {
                    if !published_topics.contains(&t.name) {
                        published_topics.push(t.name.clone());
                    }
                }
            }
        }
        if let Some(topic_ids) = graph.listens.get(m.id.as_str()) {
            for tid in topic_ids {
                if let Some(t) = graph.nodes_by_id.get(tid) {
                    if !subscribed_topics.contains(&t.name) {
                        subscribed_topics.push(t.name.clone());
                    }
                }
            }
        }
    }

    if !published_topics.is_empty() {
        md.push_str("## Publishes\n\n");
        for t in &published_topics {
            md.push_str(&format!("- `{}`\n", t));
        }
        md.push('\n');
    }

    if !subscribed_topics.is_empty() {
        md.push_str("## Subscribes\n\n");
        for t in &subscribed_topics {
            md.push_str(&format!("- `{}`\n", t));
        }
        md.push('\n');
    }

    if let Some(routes) = graph.community_routes.get(comm_id) {
        if !routes.is_empty() {
            md.push_str("## API Surface\n\n");
            md.push_str("| Method | Path |\n");
            md.push_str("|---|---|\n");
            for (_, route) in routes {
                md.push_str(&format!(
                    "| `{}` | `{}` |\n",
                    route_http_method(route),
                    route_path(route),
                ));
            }
            md.push('\n');
        }
    }

    if let Some(tables) = graph.community_db_tables.get(comm_id) {
        if !tables.is_empty() {
            md.push_str("## Data Access\n\n");
            md.push_str("| Table | Read | Write |\n");
            md.push_str("|---|---|---|\n");
            for t in tables {
                md.push_str(&format!(
                    "| `{}` | {} | {} |\n",
                    t.table_name,
                    if t.reads { "✓" } else { "" },
                    if t.writes { "✓" } else { "" },
                ));
            }
            md.push('\n');
        }
    }

    md
}

pub fn render_ba_community_json(graph: &WikiGraph, community: &Node) -> serde_json::Value {
    let comm_id = community.id.as_str();
    let empty_members: Vec<Node> = Vec::new();
    let member_list = graph
        .members_by_community
        .get(comm_id)
        .unwrap_or(&empty_members);

    let nodes: Vec<serde_json::Value> = member_list
        .iter()
        .map(|n| {
            serde_json::json!({
                "id": n.id.as_str(),
                "label": n.name.as_str(),
                "kind": n.kind.label(),
            })
        })
        .collect();

    let member_ids: std::collections::HashSet<String> =
        member_list.iter().map(|n| n.id.as_str().to_string()).collect();

    let links: Vec<serde_json::Value> = member_list
        .iter()
        .flat_map(|m| {
            let src_id = m.id.as_str().to_string();
            let empty: Vec<String> = Vec::new();
            let dsts = graph.calls_out.get(&src_id).unwrap_or(&empty);
            dsts.iter()
                .filter(|d| member_ids.contains(*d))
                .map(move |dst| {
                    serde_json::json!({
                        "source": &src_id,
                        "target": dst,
                        "label": "CALLS",
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect();

    serde_json::json!({
        "format": "community-slice",
        "community_id": comm_id,
        "nodes": nodes,
        "links": links,
    })
}

fn processes_for_community(graph: &WikiGraph, community_id: &str) -> Vec<String> {
    let mut result = Vec::new();
    for (proc_id, steps) in &graph.process_steps {
        if let Some(first) = steps.first() {
            let sym_id = first.symbol.id.as_str().to_string();
            if graph
                .community_by_member
                .get(&sym_id)
                .map(|c| c.as_str())
                == Some(community_id)
            {
                result.push(proc_id.clone());
            }
        }
    }
    result.sort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};

    fn make_node(id: &str, kind: NodeKind, name: &str) -> Node {
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

    fn member_edge(sym_id: &str, comm_id: &str) -> Edge {
        Edge {
            src: NodeId::new(sym_id.to_string()),
            dst: NodeId::new(comm_id.to_string()),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
        }
    }

    fn two_community_graph() -> WikiGraph {
        let sym_a = make_node("Method:A#doA/0", NodeKind::Method, "doA");
        let sym_b = make_node("Method:B#doB/0", NodeKind::Method, "doB");
        let comm_a = make_node("Community:0", NodeKind::Community, "svc-a");
        let comm_b = make_node("Community:1", NodeKind::Community, "svc-b");
        let nodes = [sym_a.clone(), sym_b.clone()];
        let edges = [Edge {
            src: NodeId::new("Method:A#doA/0".to_string()),
            dst: NodeId::new("Method:B#doB/0".to_string()),
            kind: EdgeKind::Calls,
            confidence: 1.0,
            reason: String::new(),
        }];
        let comm_nodes = [comm_a, comm_b];
        let comm_edges = [
            member_edge("Method:A#doA/0", "Community:0"),
            member_edge("Method:B#doB/0", "Community:1"),
        ];
        WikiGraph::build(&nodes, &edges, &comm_nodes, &comm_edges)
    }

    fn slug_map() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("Community:0".to_string(), "svc-a".to_string());
        m.insert("Community:1".to_string(), "svc-b".to_string());
        m
    }

    #[test]
    fn render_ba_community_shows_inter_community_calls() {
        let g = two_community_graph();
        let comm_a = g
            .community_nodes
            .iter()
            .find(|n| n.name == "svc-a")
            .unwrap()
            .clone();
        let md = render_ba_community(&g, &comm_a, &slug_map(), None);
        assert!(md.contains("Consumes"), "has consumes section");
        assert!(md.contains("svc-b"), "mentions callee community");
    }

    #[test]
    fn render_ba_community_writes_sidecar_shape() {
        let g = two_community_graph();
        let comm_a = g
            .community_nodes
            .iter()
            .find(|n| n.name == "svc-a")
            .unwrap()
            .clone();
        let val = render_ba_community_json(&g, &comm_a);
        assert_eq!(val["format"], "community-slice");
        assert!(val["nodes"].is_array());
        assert!(val["links"].is_array());
    }

    #[test]
    fn render_ba_community_shows_data_access_when_present() {
        let sym_a = make_node("Method:A#doA/0", NodeKind::Method, "doA");
        let dbq = make_node("DbQuery:A#SQL", NodeKind::DbQuery, "SQL");
        let tbl = make_node("DbTable:ACCOUNTS", NodeKind::DbTable, "ACCOUNTS");
        let comm_a = make_node("Community:0", NodeKind::Community, "svc-a");
        let nodes = [sym_a.clone(), dbq.clone(), tbl.clone()];
        let edges = [
            Edge {
                src: sym_a.id.clone(),
                dst: dbq.id.clone(),
                kind: EdgeKind::ExecutesQuery,
                confidence: 1.0,
                reason: String::new(),
            },
            Edge {
                src: dbq.id.clone(),
                dst: tbl.id.clone(),
                kind: EdgeKind::WritesTable,
                confidence: 1.0,
                reason: String::new(),
            },
        ];
        let comm_edges = [member_edge("Method:A#doA/0", "Community:0")];
        let g = WikiGraph::build(&nodes, &edges, &[comm_a], &comm_edges);
        let comm = g.community_nodes[0].clone();
        let mut sm = BTreeMap::new();
        sm.insert("Community:0".to_string(), "svc-a".to_string());
        let md = render_ba_community(&g, &comm, &sm, None);
        assert!(md.contains("## Data Access"), "has data access section");
        assert!(md.contains("ACCOUNTS"), "has table name");
        assert!(md.contains("✓"), "has check mark for write");
    }

    #[test]
    fn render_ba_community_omits_data_access_when_none() {
        let g = two_community_graph();
        let comm_a = g.community_nodes.iter().find(|n| n.name == "svc-a").unwrap().clone();
        let md = render_ba_community(&g, &comm_a, &slug_map(), None);
        assert!(!md.contains("## Data Access"), "no data access when no db tables");
    }

    #[test]
    fn render_ba_community_inserts_workflow_summary_when_present() {
        let g = two_community_graph();
        let comm_a = g
            .community_nodes
            .iter()
            .find(|n| n.name == "svc-a")
            .unwrap()
            .clone();
        let llm = CommunityLlmSummary {
            po: String::new(),
            ba: "Orchestrates the order workflow.".to_string(),
            dev: String::new(),
        };
        let md = render_ba_community(&g, &comm_a, &slug_map(), Some(&llm));
        assert!(md.contains("## Workflow Summary"), "has workflow summary section");
        assert!(
            md.contains("Orchestrates the order workflow"),
            "has llm text"
        );
    }
}
