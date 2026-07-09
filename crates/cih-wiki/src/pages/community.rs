use std::collections::BTreeMap;

use cih_core::{Node, NodeKind};

use crate::graph::{route_http_method, route_path, WikiGraph};
use crate::{CommunityLlmFull, CommunityLlmSummary};

fn prop_str<'a>(node: &'a Node, key: &str) -> &'a str {
    node.props
        .as_ref()
        .and_then(|p| p.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

fn prop_f64(node: &Node, key: &str) -> f64 {
    node.props
        .as_ref()
        .and_then(|p| p.get(key))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
}

fn prop_usize(node: &Node, key: &str) -> usize {
    node.props
        .as_ref()
        .and_then(|p| p.get(key))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize
}

/// Render `pages/communities/index.md` — table of all communities.
///
/// `slug_map` maps community_id → directory slug used in links.
pub fn render_community_index(
    communities: &[Node],
    slug_map: &BTreeMap<String, String>,
    graph: &WikiGraph,
) -> String {
    let mut md = String::new();
    md.push_str("---\ntitle: Communities\nsidebar_position: 5\n---\n\n");
    md.push_str("# Communities\n\n");
    md.push_str(&format!(
        "**{}** communities detected by Leiden community detection.\n\n",
        communities.len()
    ));

    if communities.is_empty() {
        md.push_str(
            "_No communities detected. Run `cih-engine discover` to build community structure._\n",
        );
        return md;
    }

    md.push_str("| Community | Stereotype | Members | Routes | Cohesion | Feature |\n");
    md.push_str("|---|---|---|---|---|---|\n");

    for comm in communities {
        let comm_id = comm.id.as_str();
        let slug = slug_map.get(comm_id).map(|s| s.as_str()).unwrap_or(comm_id);
        let stereotype = prop_str(comm, "primary_stereotype");
        let symbol_count = prop_usize(comm, "symbol_count");
        let cohesion = prop_f64(comm, "cohesion");
        let feature = prop_str(comm, "feature");
        let route_count = graph
            .community_routes
            .get(comm_id)
            .map(|r| r.len())
            .unwrap_or(0);

        md.push_str(&format!(
            "| [{}](./{}/index.md) | {} | {} | {} | {:.2} | {} |\n",
            comm.name, slug, stereotype, symbol_count, route_count, cohesion, feature,
        ));
    }
    md.push('\n');
    md
}

/// Render `pages/communities/<slug>/index.md` — structural community detail page.
///
/// Always generated (no LLM required). Includes members, cross-community dependencies,
/// and processes. LLM quick summary is appended when `llm` is provided.
pub fn render_community_detail(
    community: &Node,
    graph: &WikiGraph,
    processes: &[&Node],
    llm: Option<&CommunityLlmSummary>,
) -> String {
    let comm_id = community.id.as_str();
    let name = &community.name;
    let stereotype = prop_str(community, "primary_stereotype");
    let cohesion = prop_f64(community, "cohesion");
    let symbol_count = prop_usize(community, "symbol_count");
    let feature = prop_str(community, "feature");
    let color = prop_str(community, "color");

    let mut md = String::new();
    md.push_str(&format!(
        "---\ntitle: {name}\ncommunity_id: {comm_id}\n---\n\n"
    ));
    md.push_str(&format!("# {name}\n\n"));

    // Metadata row
    let color_badge = if !color.is_empty() {
        format!(" `{color}`")
    } else {
        String::new()
    };
    md.push_str(&format!(
        "**Stereotype:** {stereotype} · **Members:** {symbol_count} · **Cohesion:** {cohesion:.2} · **Feature:** {feature}{color_badge}\n\n",
    ));

    // Quick LLM summary (brief, always at the top when available)
    if let Some(llm) = llm {
        if !llm.dev.is_empty() || !llm.po.is_empty() {
            md.push_str("> **Summary**\n>\n");
            if !llm.po.is_empty() {
                md.push_str(&format!("> {}\n>\n", llm.po));
            }
            if !llm.dev.is_empty() {
                md.push_str(&format!("> {}\n", llm.dev));
            }
            md.push_str("\n\n");
        }
    }

    // Members table
    let members = graph.members_by_community.get(comm_id);
    if let Some(members) = members.filter(|m| !m.is_empty()) {
        md.push_str("## Members\n\n");

        // Group by kind
        let mut by_kind: BTreeMap<&str, Vec<&Node>> = BTreeMap::new();
        for m in members {
            let kind_label = match m.kind {
                NodeKind::Class | NodeKind::Interface | NodeKind::Enum | NodeKind::Record => {
                    "Class / Interface"
                }
                NodeKind::Method | NodeKind::Constructor | NodeKind::Function => {
                    "Method / Function"
                }
                NodeKind::Route => "Route",
                _ => "Other",
            };
            by_kind.entry(kind_label).or_default().push(m);
        }

        for (kind_label, nodes) in &by_kind {
            md.push_str(&format!("### {kind_label}\n\n"));
            md.push_str("| Name | File |\n");
            md.push_str("|---|---|\n");
            for node in nodes {
                let file = node
                    .props
                    .as_ref()
                    .and_then(|p| p.get("file"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                md.push_str(&format!("| {} | {} |\n", node.name, file));
            }
            md.push('\n');
        }
    } else {
        md.push_str("_No member symbols recorded for this community._\n\n");
    }

    // Routes served by this community
    if let Some(routes) = graph
        .community_routes
        .get(comm_id)
        .filter(|r| !r.is_empty())
    {
        md.push_str("## Routes\n\n");
        md.push_str("| Method | Path | Handler |\n");
        md.push_str("|---|---|---|\n");
        for (handler, route) in routes {
            md.push_str(&format!(
                "| {} | {} | {} |\n",
                route_http_method(route),
                route_path(route),
                handler.name,
            ));
        }
        md.push('\n');
    }

    // Cross-community dependencies
    let callees = graph.callees_of(comm_id);
    let callers = graph.callers_of(comm_id);
    if !callees.is_empty() || !callers.is_empty() {
        md.push_str("## Dependencies\n\n");
        if !callees.is_empty() {
            md.push_str("### Depends On\n\n");
            md.push_str("| Community | Calls |\n");
            md.push_str("|---|---|\n");
            for (target_id, count) in &callees {
                md.push_str(&format!(
                    "| {} | {} |\n",
                    graph.community_name(target_id),
                    count,
                ));
            }
            md.push('\n');
        }
        if !callers.is_empty() {
            md.push_str("### Depended On By\n\n");
            md.push_str("| Community | Calls |\n");
            md.push_str("|---|---|\n");
            for (src_id, count) in &callers {
                md.push_str(&format!(
                    "| {} | {} |\n",
                    graph.community_name(src_id),
                    count,
                ));
            }
            md.push('\n');
        }
    }

    // Processes that pass through this community
    if !processes.is_empty() {
        md.push_str("## Processes\n\n");
        md.push_str("| Process | Steps | Type | Feature |\n");
        md.push_str("|---|---|---|---|\n");
        for proc in processes {
            let proc_id = proc.id.as_str();
            let step_count = prop_usize(proc, "step_count");
            let proc_type = prop_str(proc, "process_type");
            let proc_feature = graph
                .process_steps
                .get(proc_id)
                .and_then(|steps| steps.first())
                .and_then(|s| graph.community_by_member.get(s.symbol.id.as_str()))
                .and_then(|cid| graph.nodes_by_id.get(cid.as_str()))
                .and_then(|n| n.props.as_ref())
                .and_then(|p| p.get("feature"))
                .and_then(|v| v.as_str())
                .unwrap_or(feature);
            md.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                proc.name, step_count, proc_type, proc_feature,
            ));
        }
        md.push('\n');
    }

    md
}

/// Render `pages/communities/<slug>/po.md` — Product Owner view.
///
/// Only generated when `llm_full` is available.
pub fn render_community_po(
    community: &Node,
    graph: &WikiGraph,
    llm_full: &CommunityLlmFull,
) -> String {
    let comm_id = community.id.as_str();
    let name = &community.name;
    let title = format!("{name} — Business Overview");

    let mut md = String::new();
    md.push_str(&format!(
        "---\ntitle: {title}\nsidebar_position: 1\n---\n\n"
    ));
    md.push_str(&format!("# {title}\n\n"));

    if !llm_full.po_summary.is_empty() {
        md.push_str("## Overview\n\n");
        md.push_str(&llm_full.po_summary);
        md.push_str("\n\n");
    }
    if !llm_full.po_capabilities.is_empty() {
        md.push_str("## Capabilities\n\n");
        md.push_str(&llm_full.po_capabilities);
        md.push_str("\n\n");
    }
    if !llm_full.po_workflows.is_empty() {
        md.push_str("## Workflows\n\n");
        md.push_str(&llm_full.po_workflows);
        md.push_str("\n\n");
    }
    if !llm_full.po_open_questions.is_empty() {
        md.push_str("## Open Questions\n\n");
        md.push_str(&llm_full.po_open_questions);
        md.push_str("\n\n");
    }

    // Routes summary for the PO
    if let Some(routes) = graph
        .community_routes
        .get(comm_id)
        .filter(|r| !r.is_empty())
    {
        md.push_str("## Endpoints\n\n");
        md.push_str("| Method | Path |\n");
        md.push_str("|---|---|\n");
        for (_, route) in routes {
            md.push_str(&format!(
                "| {} | {} |\n",
                route_http_method(route),
                route_path(route),
            ));
        }
        md.push('\n');
    }

    let _ = (name, graph); // suppress unused warnings if no routes
    md
}

/// Render `pages/communities/<slug>/ba.md` — Business Analyst view.
///
/// Only generated when `llm_full` is available.
pub fn render_community_ba(
    community: &Node,
    graph: &WikiGraph,
    processes: &[&Node],
    llm_full: &CommunityLlmFull,
) -> String {
    let comm_id = community.id.as_str();
    let name = &community.name;
    let title = format!("{name} — Business Analysis");

    let mut md = String::new();
    md.push_str(&format!(
        "---\ntitle: {title}\nsidebar_position: 2\n---\n\n"
    ));
    md.push_str(&format!("# {title}\n\n"));

    if !llm_full.ba_process_overview.is_empty() {
        md.push_str("## Process Overview\n\n");
        md.push_str(&llm_full.ba_process_overview);
        md.push_str("\n\n");
    }
    if !llm_full.ba_contracts.is_empty() {
        md.push_str("## Contracts\n\n");
        md.push_str(&llm_full.ba_contracts);
        md.push_str("\n\n");
    }
    if !llm_full.ba_business_rules.is_empty() {
        md.push_str("## Business Rules\n\n");
        md.push_str(&llm_full.ba_business_rules);
        md.push_str("\n\n");
    }

    // Process traces (structural, always shown)
    if !processes.is_empty() {
        md.push_str("## Flows Through This Community\n\n");
        for proc in processes {
            let proc_id = proc.id.as_str();
            let step_count = prop_usize(proc, "step_count");
            md.push_str(&format!("### {}\n\n", proc.name));
            md.push_str(&format!("**Steps:** {step_count}\n\n"));

            if let Some(steps) = graph.process_steps.get(proc_id) {
                md.push_str("| Step | Symbol | Community |\n");
                md.push_str("|---|---|---|\n");
                for step in steps {
                    let step_comm = graph
                        .community_by_member
                        .get(step.symbol.id.as_str())
                        .map(|c| graph.community_name(c))
                        .unwrap_or("—");
                    md.push_str(&format!(
                        "| {} | {} | {} |\n",
                        step.step_number, step.symbol.name, step_comm,
                    ));
                }
                md.push('\n');
            }
        }
    }

    // Messaging (publish/consume)
    let (publishes, consumes) = graph.community_messaging(comm_id);
    if !publishes.is_empty() || !consumes.is_empty() {
        md.push_str("## Messaging\n\n");
        if !publishes.is_empty() {
            md.push_str("**Publishes:** ");
            let list: Vec<String> = publishes
                .iter()
                .map(|(name, kind)| format!("`{name}` ({kind})"))
                .collect();
            md.push_str(&list.join(", "));
            md.push_str("\n\n");
        }
        if !consumes.is_empty() {
            md.push_str("**Consumes:** ");
            let list: Vec<String> = consumes
                .iter()
                .map(|(name, kind)| format!("`{name}` ({kind})"))
                .collect();
            md.push_str(&list.join(", "));
            md.push_str("\n\n");
        }
    }

    let _ = (name, graph);
    md
}
