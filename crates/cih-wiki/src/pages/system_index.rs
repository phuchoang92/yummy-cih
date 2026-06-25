use std::collections::HashSet;

use crate::features::FeatureGroup;
use crate::graph::WikiGraph;
use crate::capitalize;

/// Render the top-level system index page (pages/index.md).
/// Lists all features with their module, route, and table counts.
pub fn render_system_index(
    feature_groups: &[FeatureGroup],
    graph: &WikiGraph,
    repo_name: &str,
) -> String {
    let mut md = String::new();
    md.push_str(&format!("---\nslug: /\ntitle: {}\nsidebar_position: 1\n---\n\n", repo_name));
    md.push_str(&format!("# {}\n\n", repo_name));
    md.push_str(&format!(
        "**Features:** {} · **Modules:** {} · **Routes:** {}\n\n",
        feature_groups.len(),
        graph.community_nodes.len(),
        graph.routes.len(),
    ));

    md.push_str("## Features\n\n");
    md.push_str("| Feature | Modules | Routes | Tables |\n");
    md.push_str("|---|---|---|---|\n");

    for group in feature_groups {
        let module_count = group.community_ids.len();
        let route_count: usize = group
            .community_ids
            .iter()
            .map(|cid| {
                graph
                    .community_routes
                    .get(cid)
                    .map(|r| r.len())
                    .unwrap_or(0)
            })
            .sum();
        let mut tables: HashSet<String> = HashSet::new();
        for cid in &group.community_ids {
            if let Some(ts) = graph.community_db_tables.get(cid) {
                for t in ts {
                    tables.insert(t.table_name.clone());
                }
            }
        }
        md.push_str(&format!(
            "| [{}]({}/index.md) | {} | {} | {} |\n",
            capitalize(&group.feature),
            group.feature,
            module_count,
            route_count,
            tables.len(),
        ));
    }
    md.push('\n');

    if !graph.inter_community_calls.is_empty() {
        md.push_str("## Cross-Module Dependencies\n\n");
        md.push_str("| Caller | Callee | Calls |\n");
        md.push_str("|---|---|---|\n");
        for (src, dst, count) in &graph.inter_community_calls {
            md.push_str(&format!(
                "| {} | {} | {} |\n",
                graph.community_name(src),
                graph.community_name(dst),
                count,
            ));
        }
        md.push('\n');
    }

    if !graph.community_nodes.is_empty() {
        md.push_str("## Communities\n\n");
        md.push_str(&format!(
            "**{}** communities discovered by Leiden community detection. [Browse all communities](communities/index.md)\n\n",
            graph.community_nodes.len(),
        ));
    }

    md
}



