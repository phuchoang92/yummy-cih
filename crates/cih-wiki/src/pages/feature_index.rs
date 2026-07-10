use crate::capitalize;
use crate::graph::WikiGraph;

/// Render the feature landing page that links to po/ba pages and lists dev classes.
///
/// `class_dev_links` is a list of `(class_name, dev_slug)` pairs already sorted by name.
/// The dev slug is relative to the feature directory, e.g. `"dev/payment-controller"`.
/// All links are relative to this page (`pages/{feature}/index.md`).
pub fn render_feature_index(
    feature: &str,
    community_ids: &[String],
    class_dev_links: &[(String, String)],
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
        "**Classes:** {} · **Routes:** {} · **Methods:** {}\n\n",
        class_dev_links.len(),
        total_routes,
        total_methods,
    ));

    md.push_str("## Role Pages\n\n");
    md.push_str("- [Business Overview](po.md)\n");
    md.push_str("- [Business Analysis](ba.md)\n\n");

    if !class_dev_links.is_empty() {
        md.push_str("## Classes\n\n");
        md.push_str("| Class | Dev Page |\n");
        md.push_str("|---|---|\n");
        for (class_name, dev_slug) in class_dev_links {
            // dev_slug is relative to the feature dir, e.g. "dev/payment-controller".
            // This page is at pages/{feature}/index.md, so dev/{slug}.md resolves correctly.
            md.push_str(&format!(
                "| {} | [dev]({}.md) |\n",
                class_name, dev_slug
            ));
        }
        md.push('\n');
    }

    md
}
