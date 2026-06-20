use crate::graph::{route_http_method, route_path, WikiGraph};
use crate::CommunityLlmSummary;
use cih_core::{Node, NodeKind};
use std::collections::BTreeMap;

pub fn render_ba_index(graph: &WikiGraph) -> String {
    let mut md = String::new();
    md.push_str("---\ntitle: Workflow Overview\nrole: ba\n---\n\n");
    md.push_str("<div class=\"role-banner role-ba\"><span class=\"role-dot\"></span>Business Analyst<span class=\"role-desc\">Workflows, contracts &amp; event flows</span></div>\n\n");
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
                if pub_names.is_empty() {
                    "—".to_string()
                } else {
                    pub_names.join(", ")
                },
                if sub_names.is_empty() {
                    "—".to_string()
                } else {
                    sub_names.join(", ")
                },
            ));
        }
        md.push('\n');
    }

    md
}

pub fn render_ba_community(
    graph: &WikiGraph,
    community: &Node,
    _slug_map: &BTreeMap<String, String>,
    llm: Option<&CommunityLlmSummary>,
) -> String {
    let comm_id = community.id.as_str();

    let mut md = String::new();
    md.push_str(&format!(
        "---\ntitle: {}\nrole: ba\n---\n\n",
        community.name
    ));
    md.push_str("<div class=\"role-banner role-ba\"><span class=\"role-dot\"></span>Business Analyst<span class=\"role-desc\">Workflows, contracts &amp; event flows</span></div>\n\n");
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
                        let loc = if !step.symbol.file.is_empty()
                            && step.symbol.range.start_line > 0
                        {
                            format!(" — `{}:{}`", step.symbol.file, step.symbol.range.start_line)
                        } else if !step.symbol.file.is_empty() {
                            format!(" — `{}`", step.symbol.file)
                        } else {
                            String::new()
                        };
                        md.push_str(&format!("{}. `{}`{}\n", i + 1, step.symbol.name, loc));
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

    let member_ids: std::collections::HashSet<String> = member_list
        .iter()
        .map(|n| n.id.as_str().to_string())
        .collect();

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
            if graph.community_by_member.get(&sym_id).map(|c| c.as_str()) == Some(community_id) {
                result.push(proc_id.clone());
            }
        }
    }
    result.sort();
    result
}

#[cfg(test)]
mod tests;

