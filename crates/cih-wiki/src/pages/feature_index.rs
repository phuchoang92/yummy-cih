use std::collections::HashMap;

use crate::capitalize;
use crate::graph::WikiGraph;

/// Render the feature landing page that links to po/ba pages and lists dev modules.
pub fn render_feature_index(
    feature: &str,
    community_ids: &[String],
    dev_paths: &HashMap<String, String>,
    graph: &WikiGraph,
) -> String {
    let title = format!("{} — Feature Overview", capitalize(feature));
    let mut md = String::new();
    md.push_str(&format!(
        "---\ntitle: {}\nsidebar_position: 0\n---\n\n",
        title
    ));
    md.push_str(&format!("# {}\n\n", title));

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
    let total_methods: usize = community_ids
        .iter()
        .map(|cid| graph.community_method_counts.get(cid).copied().unwrap_or(0))
        .sum();

    md.push_str(&format!(
        "**Modules:** {} · **Routes:** {} · **Methods:** {}\n\n",
        community_ids.len(),
        total_routes,
        total_methods,
    ));

    md.push_str("## Role Pages\n\n");
    md.push_str("- [Business Overview](po.md)\n");
    md.push_str("- [Business Analysis](ba.md)\n\n");

    md.push_str("## Technical Modules\n\n");
    md.push_str("| Module | Routes | Methods | Dev Page |\n");
    md.push_str("|---|---|---|---|\n");
    for cid in community_ids {
        let comm_name = graph.community_name(cid);
        let route_count = graph
            .community_routes
            .get(cid)
            .map(|r| r.len())
            .unwrap_or(0);
        let method_count = graph.community_method_counts.get(cid).copied().unwrap_or(0);
        let dev_path = dev_paths.get(cid).cloned().unwrap_or_default();
        md.push_str(&format!(
            "| {} | {} | {} | [dev]({}.md) |\n",
            comm_name, route_count, method_count, dev_path
        ));
    }
    md.push('\n');

    md
}
