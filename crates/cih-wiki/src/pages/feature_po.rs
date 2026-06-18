use std::collections::{BTreeMap, HashMap};

use cih_core::Node;

use crate::graph::{route_http_method, route_path, WikiGraph};
use crate::slugify::slugify;
use crate::{CommunityLlmFull, CommunityLlmSummary};

fn capitalize(s: &str) -> String {
    let mut out = s.to_string();
    if let Some(first) = out.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    out
}

/// Render the feature-level PO (business overview) page.
/// Aggregates routes, tables, and LLM summaries from all communities in the feature.
pub fn render_feature_po(
    feature: &str,
    community_ids: &[String],
    graph: &WikiGraph,
    llm_summaries: Option<&HashMap<String, CommunityLlmSummary>>,
    llm_full: Option<&HashMap<String, CommunityLlmFull>>,
) -> String {
    let title = format!("{} — Business Overview", capitalize(feature));
    let mut md = String::new();
    md.push_str(&format!("---\ntitle: {}\n---\n\n", title));
    md.push_str(&format!("# {}\n\n", title));

    // llm-full mode: richer sections per community
    let full_entries: Vec<&CommunityLlmFull> = community_ids
        .iter()
        .filter_map(|cid| llm_full.and_then(|m| m.get(cid)))
        .collect();

    if !full_entries.is_empty() {
        let summaries: Vec<&str> = full_entries
            .iter()
            .map(|f| f.po_summary.as_str())
            .filter(|s| !s.is_empty())
            .collect();
        if !summaries.is_empty() {
            md.push_str("## Overview\n\n");
            for s in &summaries {
                md.push_str(s);
                md.push_str("\n\n");
            }
        }
        let caps: Vec<&str> = full_entries
            .iter()
            .map(|f| f.po_capabilities.as_str())
            .filter(|s| !s.is_empty())
            .collect();
        if !caps.is_empty() {
            md.push_str("## Capabilities\n\n");
            for s in &caps {
                md.push_str(s);
                md.push_str("\n\n");
            }
        }
        let workflows: Vec<&str> = full_entries
            .iter()
            .map(|f| f.po_workflows.as_str())
            .filter(|s| !s.is_empty())
            .collect();
        if !workflows.is_empty() {
            md.push_str("## Workflows\n\n");
            for s in &workflows {
                md.push_str(s);
                md.push_str("\n\n");
            }
        }
        let questions: Vec<&str> = full_entries
            .iter()
            .map(|f| f.po_open_questions.as_str())
            .filter(|s| !s.is_empty())
            .collect();
        if !questions.is_empty() {
            md.push_str("## Open Questions\n\n");
            for s in &questions {
                md.push_str(s);
                md.push_str("\n\n");
            }
        }
    } else {
        // llm-summary mode fallback
        let po_texts: Vec<String> = community_ids
            .iter()
            .filter_map(|cid| llm_summaries.and_then(|m| m.get(cid)).map(|s| s.po.clone()))
            .filter(|s| !s.is_empty())
            .collect();

        if !po_texts.is_empty() {
            md.push_str("## Overview\n\n");
            for text in &po_texts {
                md.push_str(text);
                md.push_str("\n\n");
            }
        }
    }

    let total_routes: usize = community_ids
        .iter()
        .map(|cid| {
            graph
                .community_routes
                .get(cid)
                .map(|r| r.len())
                .unwrap_or(0)
        })
        .sum();
    let total_procs: usize = community_ids
        .iter()
        .map(|cid| graph.processes_for_community(cid, true).len())
        .sum();

    // Aggregate messaging topics across all communities
    let mut publishes: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let mut consumes: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    for cid in community_ids {
        let (pub_topics, con_topics) = graph.community_messaging(cid);
        for (name, kind) in pub_topics {
            publishes.insert(name, kind);
        }
        for (name, kind) in con_topics {
            consumes.insert(name, kind);
        }
    }
    let total_topics = publishes.len() + consumes.len();

    let topics_part = if total_topics > 0 {
        format!(" · **Topics:** {}", total_topics)
    } else {
        String::new()
    };

    md.push_str(&format!(
        "**Modules:** {} · **Routes:** {}{} · **Processes:** {}\n\n",
        community_ids.len(),
        total_routes,
        topics_part,
        total_procs,
    ));

    // Aggregated routes
    let mut all_routes: Vec<(String, String)> = Vec::new();
    for cid in community_ids {
        if let Some(routes) = graph.community_routes.get(cid) {
            for (_, route) in routes {
                all_routes.push((
                    route_http_method(route).to_string(),
                    route_path(route).to_string(),
                ));
            }
        }
    }
    if !all_routes.is_empty() {
        md.push_str("## API Routes\n\n");
        md.push_str("| Method | Path |\n");
        md.push_str("|---|---|\n");
        for (method, path) in &all_routes {
            md.push_str(&format!("| `{}` | `{}` |\n", method, path));
        }
        md.push('\n');
    }

    // Aggregated DB tables
    let mut tables: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for cid in community_ids {
        if let Some(ts) = graph.community_db_tables.get(cid) {
            for t in ts {
                let e = tables.entry(t.table_name.clone()).or_default();
                e.0 |= t.reads;
                e.1 |= t.writes;
            }
        }
    }
    if !tables.is_empty() {
        md.push_str("## Core Tables\n\n");
        md.push_str("| Table | Access |\n");
        md.push_str("|---|---|\n");
        for (name, (reads, writes)) in &tables {
            let access = match (reads, writes) {
                (true, true) => "Read + Write",
                (true, false) => "Read",
                (false, true) => "Write",
                _ => "—",
            };
            md.push_str(&format!("| `{}` | {} |\n", name, access));
        }
        md.push('\n');
    }

    // Messaging topics
    if !publishes.is_empty() || !consumes.is_empty() {
        md.push_str("## Topics\n\n");
        md.push_str("| Direction | Topic | Type |\n");
        md.push_str("|---|---|---|\n");
        for (name, kind) in &publishes {
            md.push_str(&format!(
                "| Publishes | `{}` | {} |\n",
                name,
                capitalize(kind)
            ));
        }
        for (name, kind) in &consumes {
            md.push_str(&format!(
                "| Consumes | `{}` | {} |\n",
                name,
                capitalize(kind)
            ));
        }
        md.push('\n');
    }

    // Controllers section — one entry per controller class in this feature
    let mut feature_controllers: Vec<(&String, &Vec<(Node, Node)>)> = graph
        .routes_by_controller
        .iter()
        .filter(|(ctrl, _)| {
            graph
                .controller_feature
                .get(*ctrl)
                .map(|f| f.as_str() == feature)
                .unwrap_or(false)
        })
        .collect();
    feature_controllers.sort_by_key(|(ctrl, _)| ctrl.as_str());

    if !feature_controllers.is_empty() {
        md.push_str("## Controllers\n\n");
        md.push_str("| Controller | Routes |\n");
        md.push_str("|---|---|\n");
        for (ctrl_name, routes) in &feature_controllers {
            let slug = slugify(ctrl_name);
            md.push_str(&format!(
                "| [{}](controllers/{}.md) | {} |\n",
                ctrl_name,
                slug,
                routes.len()
            ));
        }
        md.push('\n');
    }

    md
}

/// Render a single controller's route page (PO-facing).
pub fn render_controller_page(
    controller_name: &str,
    routes: &[(Node, Node)],
    description: Option<&str>,
) -> String {
    let route_count = routes.len();
    let mut md = String::new();
    md.push_str(&format!(
        "---\ntitle: {}\nrole: po\n---\n\n",
        controller_name
    ));
    md.push_str("<div class=\"role-banner role-po\"><span class=\"role-dot\"></span>Product Owner<span class=\"role-desc\">Business capabilities &amp; stakeholder view</span></div>\n\n");
    md.push_str(&format!("# {}\n\n", controller_name));
    if let Some(desc) = description.filter(|s| !s.is_empty()) {
        md.push_str(desc);
        md.push_str("\n\n");
    }
    md.push_str(&format!(
        "**{} route{}**\n\n",
        route_count,
        if route_count == 1 { "" } else { "s" }
    ));
    md.push_str("| Method | Path | Handler |\n");
    md.push_str("|---|---|---|\n");
    for (handler, route) in routes {
        let method_name = handler_method_name(handler.id.as_str());
        md.push_str(&format!(
            "| `{}` | `{}` | `{}` |\n",
            route_http_method(route),
            route_path(route),
            method_name,
        ));
    }
    md.push('\n');
    md
}

fn handler_method_name(handler_id: &str) -> &str {
    handler_id
        .split('#')
        .nth(1)
        .and_then(|s| s.split('/').next())
        .unwrap_or(handler_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};

    fn method_node(id: &str) -> Node {
        Node {
            id: NodeId::new(id.to_string()),
            kind: NodeKind::Method,
            name: id
                .split('#')
                .nth(1)
                .unwrap_or("m")
                .split('/')
                .next()
                .unwrap_or("m")
                .to_string(),
            qualified_name: None,
            file: String::new(),
            range: Range::default(),
            props: None,
        }
    }

    fn comm_node(id: &str, name: &str) -> Node {
        Node {
            id: NodeId::new(id.to_string()),
            kind: NodeKind::Community,
            name: name.to_string(),
            qualified_name: None,
            file: String::new(),
            range: Range::default(),
            props: None,
        }
    }

    fn member_edge(method: &str, comm: &str) -> Edge {
        Edge {
            src: NodeId::new(method.to_string()),
            dst: NodeId::new(comm.to_string()),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
        }
    }

    fn simple_graph() -> (WikiGraph, Vec<String>) {
        let m = method_node("Method:A#do/0");
        let c = comm_node("Community:0", "payment");
        let g = WikiGraph::build(
            &[m.clone()],
            &[],
            &[c],
            &[member_edge(m.id.as_str(), "Community:0")],
        );
        (g, vec!["Community:0".to_string()])
    }

    #[test]
    fn renders_overview_when_llm_present() {
        let (g, ids) = simple_graph();
        let mut sums = HashMap::new();
        sums.insert(
            "Community:0".to_string(),
            CommunityLlmSummary {
                po: "Handles payment flows.".to_string(),
                ba: String::new(),
                dev: String::new(),
            },
        );
        let md = render_feature_po("payment", &ids, &g, Some(&sums), None);
        assert!(md.contains("## Overview"));
        assert!(md.contains("Handles payment flows"));
    }

    #[test]
    fn omits_overview_when_no_llm() {
        let (g, ids) = simple_graph();
        let md = render_feature_po("payment", &ids, &g, None, None);
        assert!(!md.contains("## Overview"));
    }

    #[test]
    fn has_correct_frontmatter() {
        let (g, ids) = simple_graph();
        let md = render_feature_po("payment", &ids, &g, None, None);
        assert!(md.contains("---\ntitle: Payment — Business Overview"));
    }
}
