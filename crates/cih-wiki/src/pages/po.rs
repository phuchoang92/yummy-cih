use crate::graph::{route_http_method, route_path, WikiGraph};
use crate::CommunityLlmSummary;
use cih_core::Node;
use std::collections::BTreeMap;

pub fn render_po_index(
    graph: &WikiGraph,
    slug_map: &BTreeMap<String, String>,
    llm_enriched: bool,
) -> String {
    let mut md = String::new();
    md.push_str("---\ntitle: System Overview\nrole: po\n---\n\n");
    md.push_str("<div class=\"role-banner role-po\"><span class=\"role-dot\"></span>Product Owner<span class=\"role-desc\">Business capabilities &amp; stakeholder view</span></div>\n\n");
    md.push_str("# System Overview\n\n");

    if llm_enriched {
        md.push_str("> AI enrichment active\n\n");
    }

    md.push_str(&format!(
        "**Communities:** {} · **Routes:** {} · **Processes:** {}\n\n",
        graph.community_nodes.len(),
        graph.routes.len(),
        graph.process_nodes.len(),
    ));

    if graph.community_nodes.is_empty() {
        md.push_str("No communities found. Run `discover` first.\n");
        return md;
    }

    md.push_str("## Business Capabilities\n\n");
    md.push_str("| Module | Routes | Processes | Classes |\n");
    md.push_str("|---|---|---|---|\n");

    for comm in &graph.community_nodes {
        let comm_id = comm.id.as_str();
        let slug = slug_map.get(comm_id).map(|s| s.as_str()).unwrap_or(comm_id);
        let route_count = graph
            .community_routes
            .get(comm_id)
            .map(|r| r.len())
            .unwrap_or(0);
        let process_count = processes_for_community(graph, comm_id).len();
        let class_count = graph
            .community_class_counts
            .get(comm_id)
            .copied()
            .unwrap_or(0);
        md.push_str(&format!(
            "| [{}](po/{}.md) | {} | {} | {} |\n",
            comm.name, slug, route_count, process_count, class_count
        ));
    }

    md
}

pub fn render_po_community(
    graph: &WikiGraph,
    community: &Node,
    _slug_map: &BTreeMap<String, String>,
    llm: Option<&CommunityLlmSummary>,
) -> String {
    let comm_id = community.id.as_str();

    let mut md = String::new();
    md.push_str(&format!(
        "---\ntitle: {}\nrole: po\n---\n\n",
        community.name
    ));
    md.push_str("<div class=\"role-banner role-po\"><span class=\"role-dot\"></span>Product Owner<span class=\"role-desc\">Business capabilities &amp; stakeholder view</span></div>\n\n");
    md.push_str(&format!("# {}\n\n", community.name));

    if let Some(summary) = llm {
        if !summary.po.is_empty() {
            md.push_str("## Overview\n\n");
            md.push_str(&summary.po);
            md.push_str("\n\n");
        }
    }

    let class_count = graph
        .community_class_counts
        .get(comm_id)
        .copied()
        .unwrap_or(0);
    let method_count = graph
        .community_method_counts
        .get(comm_id)
        .copied()
        .unwrap_or(0);
    let route_count = graph
        .community_routes
        .get(comm_id)
        .map(|r| r.len())
        .unwrap_or(0);
    let test_count = graph
        .community_tests
        .get(comm_id)
        .map(|t| t.len())
        .unwrap_or(0);

    md.push_str(&format!(
        "**Symbols:** {} · **Routes:** {} · **Tests:** {}\n\n",
        class_count + method_count,
        route_count,
        test_count
    ));

    if let Some(routes) = graph.community_routes.get(comm_id) {
        if !routes.is_empty() {
            md.push_str("## Business Capabilities (Routes)\n\n");
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
            md.push_str("## Core Tables\n\n");
            md.push_str("| Table | Access |\n");
            md.push_str("|---|---|\n");
            for t in tables {
                let access = match (t.reads, t.writes) {
                    (true, true) => "Read + Write",
                    (true, false) => "Read",
                    (false, true) => "Write",
                    _ => "—",
                };
                md.push_str(&format!("| `{}` | {} |\n", t.table_name, access));
            }
            md.push('\n');
        }
    }

    let procs = processes_for_community(graph, comm_id);
    if !procs.is_empty() {
        md.push_str("## Business Processes\n\n");
        for proc_id in procs {
            if let Some(proc_node) = graph.nodes_by_id.get(&proc_id) {
                md.push_str(&format!("### {}\n\n", proc_node.name));
                if let Some(steps) = graph.process_steps.get(&proc_id) {
                    let chain: Vec<&str> = steps.iter().map(|s| s.symbol.name.as_str()).collect();
                    md.push_str(&chain.join(" → "));
                    md.push_str("\n\n");
                }
            }
        }
    }

    if test_count > 0 {
        md.push_str("## Test Coverage\n\n");
        md.push_str(&format!(
            "{} test class(es) covering this module.\n\n",
            test_count
        ));
    }

    md
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

