use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use crate::graph::{route_http_method, route_path, WikiGraph};
use crate::FlowLlmSummary;

/// camelCase method name from a handler node ID → "Title Case Words"
pub fn handler_title(handler_id: &str) -> String {
    let method_name = handler_id
        .split('#')
        .nth(1)
        .and_then(|s| s.split('/').next())
        .unwrap_or(handler_id);
    let mut out = String::new();
    for (i, ch) in method_name.chars().enumerate() {
        if i > 0 && ch.is_uppercase() {
            out.push(' ');
        }
        if i == 0 {
            out.extend(ch.to_uppercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// camelCase method name from a handler node ID → "kebab-case"
pub fn handler_slug(handler_id: &str) -> String {
    let method_name = handler_id
        .split('#')
        .nth(1)
        .and_then(|s| s.split('/').next())
        .unwrap_or(handler_id);
    let mut out = String::new();
    for (i, ch) in method_name.chars().enumerate() {
        if i > 0 && ch.is_uppercase() {
            out.push('-');
        }
        out.push(ch.to_ascii_lowercase());
    }
    if out.is_empty() { "flow".to_string() } else { out }
}

fn method_name_from_id(method_id: &str) -> &str {
    method_id
        .split('#')
        .nth(1)
        .and_then(|s| s.split('/').next())
        .unwrap_or(method_id)
}

fn class_simple_name_from_method_id(method_id: &str) -> &str {
    let without_kind = method_id
        .strip_prefix("Method:")
        .or_else(|| method_id.strip_prefix("Constructor:"))
        .or_else(|| method_id.strip_prefix("Function:"))
        .unwrap_or(method_id);
    let fqcn = without_kind.split('#').next().unwrap_or(without_kind);
    fqcn.rsplit('.').next().unwrap_or(fqcn)
}

fn class_id_from_method_id(method_id: &str, graph: &WikiGraph) -> String {
    let without_kind = method_id
        .strip_prefix("Method:")
        .or_else(|| method_id.strip_prefix("Constructor:"))
        .or_else(|| method_id.strip_prefix("Function:"))
        .unwrap_or(method_id);
    let fqcn = without_kind.split('#').next().unwrap_or(without_kind);
    for prefix in &["Class:", "Interface:", "Enum:", "Record:"] {
        let candidate = format!("{}{}", prefix, fqcn);
        if graph.nodes_by_id.contains_key(candidate.as_str())
            || graph.methods_by_class.contains_key(candidate.as_str())
        {
            return candidate;
        }
    }
    format!("Class:{}", fqcn)
}

/// BFS from handler through calls_out, returning method node IDs in traversal order.
/// Only includes methods whose class exists in the project graph.
fn build_call_chain(start_id: &str, graph: &WikiGraph, max_depth: usize) -> Vec<String> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut chain: Vec<String> = Vec::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    queue.push_back((start_id.to_string(), 0));
    while let Some((id, depth)) = queue.pop_front() {
        if visited.contains(&id) || depth > max_depth {
            continue;
        }
        visited.insert(id.clone());
        // Only include methods whose class is known in the project graph.
        let cls_id = class_id_from_method_id(id.as_str(), graph);
        if graph.nodes_by_id.contains_key(cls_id.as_str())
            || graph.methods_by_class.contains_key(cls_id.as_str())
        {
            chain.push(id.clone());
        }
        if let Some(callees) = graph.calls_out.get(id.as_str()) {
            for callee in callees {
                if !visited.contains(callee) {
                    queue.push_back((callee.clone(), depth + 1));
                }
            }
        }
    }
    chain
}

/// DB table access for a single method node ID.
fn db_access(method_id: &str, graph: &WikiGraph) -> Vec<(String, bool, bool)> {
    let mut tables: HashMap<String, (bool, bool)> = HashMap::new();
    if let Some(query_ids) = graph.executes_query.get(method_id) {
        for qid in query_ids {
            for tid in graph.query_reads_table.get(qid.as_str()).into_iter().flatten() {
                let name = graph.nodes_by_id.get(tid.as_str())
                    .map(|n| n.name.clone())
                    .unwrap_or_else(|| tid.clone());
                tables.entry(name).or_default().0 = true;
            }
            for tid in graph.query_writes_table.get(qid.as_str()).into_iter().flatten() {
                let name = graph.nodes_by_id.get(tid.as_str())
                    .map(|n| n.name.clone())
                    .unwrap_or_else(|| tid.clone());
                tables.entry(name).or_default().1 = true;
            }
        }
    }
    let mut v: Vec<(String, bool, bool)> =
        tables.into_iter().map(|(n, (r, w))| (n, r, w)).collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

pub fn render_api_flow_page(
    handler: &cih_core::Node,
    route: &cih_core::Node,
    position: usize,
    flow_summary: Option<&FlowLlmSummary>,
    graph: &WikiGraph,
    class_dev_slugs: &HashMap<String, String>,
    method_desc: &HashMap<String, String>,
) -> String {
    let http_method = route_http_method(route);
    let path = route_path(route);
    let title = handler_title(handler.id.as_str());

    let mut md = String::new();
    md.push_str(&format!(
        "---\ntitle: {}\nsidebar_position: {}\nrole: po\n---\n\n",
        title, position
    ));
    md.push_str("<div class=\"role-banner role-po\"><span class=\"role-dot\"></span>Product Owner<span class=\"role-desc\">Business capabilities &amp; stakeholder view</span></div>\n\n");
    md.push_str(&format!("# {}\n\n", title));
    md.push_str(&format!("> `{}` `{}`\n\n", http_method, path));

    // Business impact (LLM).
    if let Some(fs) = flow_summary {
        if !fs.business_impact.is_empty() {
            md.push_str(&fs.business_impact);
            md.push_str("\n\n");
        }
    }
    // Handler-level description from method_desc (controller LLM enrichment).
    if let Some(desc) = method_desc.get(handler.id.as_str()) {
        if !desc.is_empty() {
            md.push_str(desc);
            md.push_str("\n\n");
        }
    }

    // Call chain via BFS from handler through calls_out.
    let chain = build_call_chain(handler.id.as_str(), graph, 4);

    if !chain.is_empty() {
        md.push_str("## Flow\n\n");

        // LLM narrative if available.
        if let Some(fs) = flow_summary {
            if !fs.narrative.is_empty() {
                md.push_str(&fs.narrative);
                md.push_str("\n\n");
            }
        }

        // Collect per-step DB access.
        let step_dbs: Vec<Vec<(String, bool, bool)>> =
            chain.iter().map(|id| db_access(id.as_str(), graph)).collect();
        let has_db = step_dbs.iter().any(|v| !v.is_empty());

        if has_db {
            md.push_str("| # | Class | Method | What it does | DB access |\n");
            md.push_str("|---|---|---|---|---|\n");
        } else {
            md.push_str("| # | Class | Method | What it does |\n");
            md.push_str("|---|---|---|---|\n");
        }

        let mut seen_class_ids: Vec<String> = Vec::new();
        for (i, mid) in chain.iter().enumerate() {
            let cls = class_simple_name_from_method_id(mid.as_str());
            let meth = method_name_from_id(mid.as_str());
            let cls_id = class_id_from_method_id(mid.as_str(), graph);
            if !seen_class_ids.contains(&cls_id) {
                seen_class_ids.push(cls_id);
            }

            // Description: prefer flow step_descriptions, fall back to method_desc.
            let desc = flow_summary
                .and_then(|fs| fs.step_descriptions.get(i))
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| method_desc.get(mid.as_str()).map(|s| s.as_str()))
                .unwrap_or("");

            if has_db {
                let db_str = if step_dbs[i].is_empty() {
                    "—".to_string()
                } else {
                    step_dbs[i]
                        .iter()
                        .map(|(name, r, w)| match (r, w) {
                            (true, true) => format!("`{}` R+W", name),
                            (true, false) => format!("`{}` R", name),
                            (false, true) => format!("`{}` W", name),
                            _ => format!("`{}`", name),
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                md.push_str(&format!(
                    "| {} | `{}` | `{}` | {} | {} |\n",
                    i + 1, cls, meth, desc, db_str
                ));
            } else {
                md.push_str(&format!(
                    "| {} | `{}` | `{}` | {} |\n",
                    i + 1, cls, meth, desc
                ));
            }
        }
        md.push('\n');

        // Events published by any method in the chain.
        let mut published: BTreeMap<String, ()> = BTreeMap::new();
        for mid in &chain {
            for eid in graph.publishes.get(mid.as_str()).into_iter().flatten() {
                let name = graph.nodes_by_id.get(eid.as_str())
                    .map(|n| n.name.clone())
                    .unwrap_or_else(|| eid.clone());
                published.insert(name, ());
            }
        }
        if !published.is_empty() {
            md.push_str("## Events\n\n");
            md.push_str("| Direction | Topic |\n");
            md.push_str("|---|---|\n");
            for topic in published.keys() {
                md.push_str(&format!("| Publishes | `{}` |\n", topic));
            }
            md.push('\n');
        }

        // Technical Reference links — relative ../../dev/{slug} from api/{ctrl}/{handler}.
        let ref_links: Vec<String> = seen_class_ids
            .iter()
            .filter_map(|cls_id| {
                class_dev_slugs.get(cls_id.as_str()).map(|slug| {
                    let simple = cls_id
                        .trim_start_matches("Class:")
                        .trim_start_matches("Interface:")
                        .trim_start_matches("Enum:")
                        .trim_start_matches("Record:")
                        .rsplit('.')
                        .next()
                        .unwrap_or(cls_id.as_str());
                    format!("- [{}](../../dev/{}.md)", simple, slug)
                })
            })
            .collect();

        if !ref_links.is_empty() {
            md.push_str("## Technical Reference\n\n");
            for link in &ref_links {
                md.push_str(link);
                md.push('\n');
            }
            md.push('\n');
        }
    }

    md
}
