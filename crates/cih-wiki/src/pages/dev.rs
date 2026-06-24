use crate::bodies::BodyEntry;
use crate::graph::{node_stereotype, route_http_method, route_path, WikiGraph};
use crate::mermaid;
use crate::{CommunityLlmFull, CommunityLlmSummary};
use cih_core::{Node, NodeKind, RepoMap};
use std::collections::{BTreeMap, HashMap};

/// Strip a trailing `-N` numeric suffix from a slug, returning (base, suffix).
/// E.g. `"admin-customer-service-2"` → `("admin-customer-service", Some(2))`.
fn strip_numeric_suffix(slug: &str) -> (&str, Option<u32>) {
    if let Some(pos) = slug.rfind('-') {
        let tail = &slug[pos + 1..];
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = tail.parse::<u32>() {
                return (&slug[..pos], Some(n));
            }
        }
    }
    (slug, None)
}

fn method_signature(node: &Node) -> String {
    let params = node
        .props
        .as_ref()
        .and_then(|p| p.get("paramTypes"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    format!("{}({})", node.name, params)
}

fn callee_display(node: &Node) -> String {
    if let Some(qn) = &node.qualified_name {
        if let Some(hash_pos) = qn.find('#') {
            let class_part = &qn[..hash_pos];
            let simple = class_part.rsplit('.').next().unwrap_or(class_part);
            return format!("{}.{}", simple, node.name);
        }
    }
    node.name.clone()
}

fn format_return_type(node: &Node) -> &str {
    node.props
        .as_ref()
        .and_then(|p| p.get("returnType"))
        .and_then(|v| v.as_str())
        .unwrap_or("void")
}

pub fn render_dev_index(
    graph: &WikiGraph,
    repo_map: Option<&RepoMap>,
    unresolved_report: Option<&str>,
) -> String {
    let mut md = String::new();
    md.push_str("---\ntitle: Technical Overview\nrole: dev\n---\n\n");
    md.push_str("<div class=\"role-banner role-dev\"><span class=\"role-dot\"></span>Developer<span class=\"role-desc\">Technical structure, calls &amp; tests</span></div>\n\n");
    md.push_str("# Technical Overview\n\n");

    md.push_str("## Community Summary\n\n");
    md.push_str("| Module | Classes | Methods | Routes | Tests |\n");
    md.push_str("|---|---|---|---|---|\n");

    for comm in &graph.community_nodes {
        let comm_id = comm.id.as_str();
        let classes = graph
            .community_class_counts
            .get(comm_id)
            .copied()
            .unwrap_or(0);
        let methods = graph
            .community_method_counts
            .get(comm_id)
            .copied()
            .unwrap_or(0);
        let routes = graph
            .community_routes
            .get(comm_id)
            .map(|r| r.len())
            .unwrap_or(0);
        let tests = graph
            .community_tests
            .get(comm_id)
            .map(|t| t.len())
            .unwrap_or(0);
        md.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            comm.name, classes, methods, routes, tests,
        ));
    }
    md.push('\n');

    if let Some(rm) = repo_map {
        if !rm.modules.is_empty() {
            md.push_str("## Modules\n\n");
            md.push_str("| Module | Path |\n");
            md.push_str("|---|---|\n");
            for m in &rm.modules {
                md.push_str(&format!("| `{}` | `{}` |\n", m.name, m.rel_path));
            }
            md.push('\n');
        }

        if !rm.jars.is_empty() {
            md.push_str("## JAR Dependencies\n\n");
            md.push_str(&format!("{} external JARs detected.\n\n", rm.jars.len()));
        }
    }

    if let Some(report) = unresolved_report {
        md.push_str("## Unresolved References\n\n");
        md.push_str("> Source: `unresolved-refs.md`\n\n");
        let lines: Vec<&str> = report.lines().take(40).collect();
        for line in &lines {
            md.push_str(line);
            md.push('\n');
        }
        if report.lines().count() > 40 {
            md.push_str("\n_(truncated — see `unresolved-refs.md` for full report)_\n");
        }
        md.push('\n');
    }

    md
}

fn cap_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

fn slug_to_title(slug: &str) -> String {
    slug.split('-').map(cap_first).collect::<Vec<_>>().join(" ")
}

/// Normalize a name or slug to alphanumeric-only lowercase for comparison.
/// Handles both camelCase (`FineractFeignClient`) and kebab (`fineract-feign-client`).
fn name_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// When a community page has a numeric suffix (e.g. `fineract-feign-client-2`),
/// look at the community's members for a more distinctive class to use as the title.
/// Returns None when all members belong to the same (shredded) base class.
fn distinctive_class_name(graph: &WikiGraph, community: &Node, base_slug: &str) -> Option<String> {
    let base_key = name_key(base_slug);
    let comm_id = community.id.as_str();
    let empty: Vec<Node> = Vec::new();
    let members = graph.members_by_community.get(comm_id).unwrap_or(&empty);

    let mut class_info: HashMap<String, (String, usize, bool)> = HashMap::new();
    for m in members {
        if !matches!(m.kind, NodeKind::Method | NodeKind::Constructor | NodeKind::Function) {
            continue;
        }
        let Some(cls_id) = m.id.as_str().split_once('#').map(|(prefix, _)| {
            let fqcn = prefix
                .trim_start_matches("Method:")
                .trim_start_matches("Constructor:");
            format!("Class:{}", fqcn)
        }) else {
            continue;
        };
        let Some(cls_node) = graph.nodes_by_id.get(&cls_id) else {
            continue;
        };
        // Normalize both sides: handles camelCase vs kebab mismatch
        if name_key(&cls_node.name) == base_key {
            continue;
        }
        let entry = class_info.entry(cls_id).or_insert_with(|| {
            let stereo = node_stereotype(cls_node);
            let is_test = stereo == Some("test");
            let has_domain = !is_test && stereo.is_some();
            (cls_node.name.clone(), 0usize, has_domain)
        });
        entry.1 += 1;
    }

    // Prefer domain-stereotyped classes (service/component/controller), then most methods.
    class_info
        .into_values()
        .filter(|(name, _, _)| {
            !name.ends_with("Test") && !name.ends_with("Tests") && !name.ends_with("Spec")
        })
        .max_by(|a, b| a.2.cmp(&b.2).then(a.1.cmp(&b.1)))
        .map(|(name, _, _)| name)
}

/// For a single-class shredded community, pick the method with the most outgoing calls
/// as a hint to distinguish this fragment. E.g. `verifyDelinquencyAction`.
fn lead_method_hint(graph: &WikiGraph, community: &Node) -> Option<String> {
    let comm_id = community.id.as_str();
    let empty: Vec<Node> = Vec::new();
    let members = graph.members_by_community.get(comm_id).unwrap_or(&empty);

    members
        .iter()
        .filter(|m| matches!(m.kind, NodeKind::Method))
        .filter(|m| !m.name.starts_with("get") && !m.name.starts_with("set") && !m.name.starts_with("is"))
        .max_by_key(|m| {
            graph.calls_out.get(m.id.as_str()).map(|v| v.len()).unwrap_or(0)
        })
        .map(|m| m.name.clone())
}

/// Build the sidebar/manifest title for a community dev page.
pub fn community_display_title(graph: &WikiGraph, community: &Node, page_path: &str) -> String {
    let slug = page_path.split('/').last().unwrap_or(&community.name);
    let (base_slug, suffix) = strip_numeric_suffix(slug);

    let primary_stereotype = community
        .props
        .as_ref()
        .and_then(|p| p.get("primary_stereotype"))
        .and_then(|v| v.as_str())
        .map(cap_first);

    // Numbered duplicate page — try to find a more meaningful title from the community members.
    if suffix.is_some() {
        // Case A: multiple classes — use the non-shared distinctive class name.
        if let Some(distinctive) = distinctive_class_name(graph, community, base_slug) {
            return match primary_stereotype {
                Some(s) => format!("{} · {}", distinctive, s),
                None => distinctive,
            };
        }
        // Case B: single class shredded across communities — append lead method as hint.
        if let Some(hint) = lead_method_hint(graph, community) {
            let base_name = slug_to_title(base_slug);
            return match primary_stereotype {
                Some(s) => format!("{} · {} · {}", base_name, hint, s),
                None => format!("{} · {}", base_name, hint),
            };
        }
    }

    let base_name = slug_to_title(base_slug);
    match primary_stereotype {
        Some(s) => format!("{} · {}", base_name, s),
        None => base_name,
    }
}

fn lang_tag(file: &str) -> &str {
    match file.rfind('.').map(|i| &file[i + 1..]).unwrap_or("") {
        "java" => "java",
        "ts" | "tsx" => "typescript",
        "py" => "python",
        _ => "",
    }
}

/// Encode `<` and `>` so MDX/JSX doesn't treat Java generics as JSX tags.
fn html_encode_angles(s: &str) -> String {
    s.replace('<', "&lt;").replace('>', "&gt;")
}

/// `page_path` is the full path without "pages/" prefix, e.g. `"payment/dev/payment-controller"`.
pub fn render_dev_community(
    graph: &WikiGraph,
    community: &Node,
    page_path: &str,
    llm: Option<&CommunityLlmSummary>,
    llm_full: Option<&CommunityLlmFull>,
    bodies: &HashMap<String, BodyEntry>,
) -> String {
    let comm_id = community.id.as_str();
    let page_title = community_display_title(graph, community, page_path);

    let mut md = String::new();
    md.push_str(&format!("---\ntitle: {}\nrole: dev\n---\n\n", page_title));
    md.push_str("<div class=\"role-banner role-dev\"><span class=\"role-dot\"></span>Developer<span class=\"role-desc\">Technical structure, calls &amp; tests</span></div>\n\n");
    md.push_str(&format!("# {} — Technical Reference\n\n", page_title));

    if let Some(full) = llm_full {
        if !full.dev_responsibility.is_empty() {
            md.push_str("## Responsibility\n\n");
            md.push_str(&full.dev_responsibility);
            md.push_str("\n\n");
        }
        if !full.dev_key_classes.is_empty() {
            md.push_str("## Key Classes\n\n");
            md.push_str(&full.dev_key_classes);
            md.push_str("\n\n");
        }
        if !full.dev_entry_points.is_empty() {
            md.push_str("## Entry Points\n\n");
            md.push_str(&full.dev_entry_points);
            md.push_str("\n\n");
        }
    } else if let Some(summary) = llm {
        if !summary.dev.is_empty() {
            md.push_str("## Summary\n\n");
            md.push_str(&summary.dev);
            md.push_str("\n\n");
        }
    }

    // Class-level call diagram: shows which classes this community calls (and is called by).
    // Operates on class-to-class edges rather than community-to-community, so it correctly
    // shows controller→service relationships even when Louvain co-locates them.
    if let Some(diagram) = mermaid::class_call_diagram(graph, comm_id) {
        md.push_str("## Class Interactions\n\n");
        md.push_str("```mermaid\n");
        md.push_str(&diagram);
        md.push_str("```\n\n");
    }

    let empty_members: Vec<Node> = Vec::new();
    let member_list = graph
        .members_by_community
        .get(comm_id)
        .unwrap_or(&empty_members);

    // Communities group methods (not classes); derive the parent class from each method's ID.
    // Method id format: "Method:fqcn#name/arity" → class id: "Class:fqcn"
    let mut class_to_methods: BTreeMap<String, Vec<&Node>> = BTreeMap::new();
    for m in member_list {
        if !matches!(
            m.kind,
            NodeKind::Method | NodeKind::Function | NodeKind::Constructor
        ) {
            continue;
        }
        let cls_id =
            m.id.as_str()
                .split_once('#')
                .map(|(prefix, _)| {
                    let fqcn = prefix
                        .trim_start_matches("Method:")
                        .trim_start_matches("Constructor:");
                    format!("Class:{}", fqcn)
                })
                .unwrap_or_default();
        if !cls_id.is_empty() {
            class_to_methods.entry(cls_id).or_default().push(m);
        }
    }

    // Separate test classes from production classes upfront so tests never appear
    // as peer sections in ## Classes — they get their own ## Tests section.
    let mut test_class_entries: Vec<(&str, &Vec<&Node>)> = Vec::new();

    if !class_to_methods.is_empty() {
        let has_non_test = class_to_methods.iter().any(|(cls_id, _)| {
            graph.nodes_by_id.get(cls_id)
                .and_then(node_stereotype)
                .map(|s| s != "test")
                .unwrap_or(true)
        });
        if has_non_test {
            md.push_str("## Classes\n\n");
        }

        for (cls_id, methods) in &class_to_methods {
            let cls = graph.nodes_by_id.get(cls_id);
            let cls_name = cls.map(|n| n.name.as_str()).unwrap_or_else(|| {
                cls_id
                    .trim_start_matches("Class:")
                    .rsplit('.')
                    .next()
                    .unwrap_or(cls_id)
            });
            let stereotype = cls.and_then(node_stereotype).unwrap_or("—");

            if stereotype == "test" {
                test_class_entries.push((cls_name, methods));
                continue;
            }

            let test_names: Vec<&str> = cls
                .map(|c| {
                    graph
                        .tests_in
                        .get(c.id.as_str())
                        .into_iter()
                        .flatten()
                        .filter_map(|id| graph.nodes_by_id.get(id).map(|n| n.name.as_str()))
                        .collect()
                })
                .unwrap_or_default();

            md.push_str(&format!("### `{}` · {}\n\n", cls_name, stereotype));

            let file = cls
                .map(|c| c.file.as_str())
                .unwrap_or_else(|| methods.first().map(|m| m.file.as_str()).unwrap_or(""));
            if !file.is_empty() {
                let line = cls.map(|c| c.range.start_line).unwrap_or(0);
                if line > 0 {
                    md.push_str(&format!("`{}` :{}\n\n", file, line));
                } else {
                    md.push_str(&format!("`{}`\n\n", file));
                }
            }

            if !test_names.is_empty() {
                md.push_str(&format!("Tests: {}\n\n", test_names.join(", ")));
            }

            let visible: Vec<&&Node> = methods
                .iter()
                .filter(|m| !matches!(m.kind, NodeKind::Constructor))
                .collect();

            if !visible.is_empty() {
                md.push_str("| Method | Returns | Line | Calls |\n");
                md.push_str("|---|---|---|---|\n");
                for method in visible.iter().take(20) {
                    let sig = method_signature(method);
                    let ret = format_return_type(method);
                    let line = if method.range.start_line > 0 {
                        format!(":{}", method.range.start_line)
                    } else {
                        String::new()
                    };
                    let empty_calls: Vec<String> = Vec::new();
                    let calls_display = graph
                        .calls_out
                        .get(method.id.as_str())
                        .unwrap_or(&empty_calls)
                        .iter()
                        .take(3)
                        .filter_map(|cid| graph.nodes_by_id.get(cid))
                        .map(callee_display)
                        .collect::<Vec<_>>()
                        .join(", ");
                    md.push_str(&format!(
                        "| `{}` | `{}` | {} | {} |\n",
                        sig, ret, line, calls_display
                    ));
                }
                if visible.len() > 20 {
                    md.push_str(&format!("\n_…and {} more methods_\n", visible.len() - 20));
                }
                md.push('\n');
            }

            // Collapsible body blocks — skip entirely for interface/abstract classes whose
            // "bodies" are just the signature declaration line.
            let is_interface = matches!(cls.map(|n| n.kind), Some(NodeKind::Interface));
            for method in methods.iter() {
                if is_interface {
                    continue;
                }
                let Some(body) = bodies.get(method.id.as_str()) else {
                    continue;
                };
                let sig = html_encode_angles(&method_signature(method));
                let lang = lang_tag(&method.file);
                let location = if method.range.start_line > 0 && method.range.end_line > 0 {
                    format!(" — lines {}–{}", method.range.start_line, method.range.end_line)
                } else {
                    String::new()
                };

                if body.original_lines <= 80 {
                    // Path A: short method — show stripped body, header only when lines removed
                    let stripped_lines = body.stripped.trim().lines().count();
                    let code_content = if stripped_lines < body.original_lines {
                        let comment_prefix = if lang == "python" { "#" } else { "//" };
                        format!(
                            "{} stripped · {} of {} lines shown\n{}",
                            comment_prefix, stripped_lines, body.original_lines, body.stripped.trim()
                        )
                    } else {
                        body.stripped.trim().to_string()
                    };
                    md.push_str(&format!(
                        "<details>\n<summary><code>{}</code>{}</summary>\n\n",
                        sig, location
                    ));
                    if lang.is_empty() {
                        md.push_str(&format!("```\n{}\n```\n\n", code_content));
                    } else {
                        md.push_str(&format!("```{}\n{}\n```\n\n", lang, code_content));
                    }
                    md.push_str("</details>\n\n");
                } else {
                    // Path B: god function — show first 30 stripped lines so the flow is visible
                    let preview: Vec<&str> = body.stripped.lines().take(30).collect();
                    if !preview.is_empty() {
                        let comment_prefix = if lang == "python" { "#" } else { "//" };
                        let code_content = format!(
                            "{} god function · {} lines — first 30 stripped lines shown\n{}",
                            comment_prefix,
                            body.original_lines,
                            preview.join("\n")
                        );
                        md.push_str(&format!(
                            "<details>\n<summary><code>{}</code>{} ⚠ large method</summary>\n\n",
                            sig, location
                        ));
                        if lang.is_empty() {
                            md.push_str(&format!("```\n{}\n```\n\n", code_content));
                        } else {
                            md.push_str(&format!("```{}\n{}\n```\n\n", lang, code_content));
                        }
                        md.push_str("</details>\n\n");
                    }
                }
            }
        }
    }

    if let Some(routes) = graph.community_routes.get(comm_id) {
        if !routes.is_empty() {
            md.push_str("## Routes\n\n");
            md.push_str("| Method | Path | Handler |\n");
            md.push_str("|---|---|---|\n");
            for (handler, route) in routes {
                md.push_str(&format!(
                    "| `{}` | `{}` | `{}` |\n",
                    route_http_method(route),
                    route_path(route),
                    handler.name,
                ));
            }
            md.push('\n');
        }
    }

    if let Some(tables) = graph.community_db_tables.get(comm_id) {
        if !tables.is_empty() {
            md.push_str("## DB Access\n\n");
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

    let mut ext_call_names: Vec<String> = Vec::new();
    for m in member_list {
        if let Some(ext_ids) = graph.external_calls.get(m.id.as_str()) {
            for eid in ext_ids {
                if let Some(ext_node) = graph.nodes_by_id.get(eid) {
                    if !ext_call_names.contains(&ext_node.name) {
                        ext_call_names.push(ext_node.name.clone());
                    }
                }
            }
        }
    }
    if !ext_call_names.is_empty() {
        md.push_str("## External Calls\n\n");
        for name in &ext_call_names {
            md.push_str(&format!("- `{}`\n", name));
        }
        md.push('\n');
    }

    if !test_class_entries.is_empty() {
        md.push_str("## Tests\n\n");
        for (cls_name, test_methods) in &test_class_entries {
            md.push_str(&format!("**`{}`**\n\n", cls_name));
            let test_fns: Vec<&str> = test_methods
                .iter()
                .filter(|m| matches!(m.kind, NodeKind::Method))
                .map(|m| m.name.as_str())
                .collect();
            if !test_fns.is_empty() {
                for name in &test_fns {
                    md.push_str(&format!("- `{}`\n", name));
                }
                md.push('\n');
            }
        }
    } else if let Some(test_ids) = graph.community_tests.get(comm_id) {
        if !test_ids.is_empty() {
            md.push_str("## Tests\n\n");
            for tid in test_ids {
                if let Some(test_node) = graph.nodes_by_id.get(tid) {
                    md.push_str(&format!("- `{}`\n", test_node.name));
                }
            }
            md.push('\n');
        }
    }

    let mut files: Vec<&str> = member_list
        .iter()
        .filter(|n| !n.file.is_empty())
        .map(|n| n.file.as_str())
        .collect();
    files.sort_unstable();
    files.dedup();

    if !files.is_empty() {
        md.push_str("## Important Files\n\n");
        for f in files.iter().take(10) {
            md.push_str(&format!("- `{}`\n", f));
        }
        if files.len() > 10 {
            md.push_str(&format!("- _…and {} more_\n", files.len() - 10));
        }
        md.push('\n');
    }

    md
}

/// Render a full dev page for a single class (all methods, routes, DB access, tests).
pub fn render_dev_class(
    graph: &WikiGraph,
    cls: &Node,
    bodies: &HashMap<String, BodyEntry>,
    method_desc: &HashMap<String, String>,
) -> String {
    let cls_id = cls.id.as_str();
    let cls_name = &cls.name;
    let stereotype = node_stereotype(cls).unwrap_or("—");
    let is_interface = matches!(cls.kind, NodeKind::Interface);

    let empty_methods: Vec<Node> = Vec::new();
    let all_methods = graph.methods_by_class.get(cls_id).unwrap_or(&empty_methods);

    let mut md = String::new();
    md.push_str(&format!("---\ntitle: {}\nrole: dev\n---\n\n", cls_name));
    md.push_str("<div class=\"role-banner role-dev\"><span class=\"role-dot\"></span>Developer<span class=\"role-desc\">Technical structure, calls &amp; tests</span></div>\n\n");
    md.push_str(&format!("# {} — Technical Reference\n\n", cls_name));

    if let Some(diagram) = mermaid::class_call_diagram_for_class(graph, cls_id) {
        md.push_str("## Class Interactions\n\n");
        md.push_str("```mermaid\n");
        md.push_str(&diagram);
        md.push_str("```\n\n");
    }

    let visible: Vec<&Node> = all_methods
        .iter()
        .filter(|m| !matches!(m.kind, NodeKind::Constructor))
        .collect();

    if !visible.is_empty() {
        md.push_str("## Methods\n\n");

        // File reference for the class (one line, no subsection heading)
        let file = if !cls.file.is_empty() {
            cls.file.as_str()
        } else {
            visible.first().map(|m| m.file.as_str()).unwrap_or("")
        };
        if !file.is_empty() {
            let stereo_part = if stereotype != "—" {
                format!(" · _{}_", stereotype)
            } else {
                String::new()
            };
            if cls.range.start_line > 0 {
                md.push_str(&format!("`{}` :{}{}  \n\n", file, cls.range.start_line, stereo_part));
            } else {
                md.push_str(&format!("`{}`{}  \n\n", file, stereo_part));
            }
        }

        // Per-method sections: each method is an H3 so it appears in the right-nav TOC.
        for (idx, method) in visible.iter().enumerate() {
            let sig = method_signature(method);
            let ret = format_return_type(method);
            let lang = lang_tag(&method.file);

            if idx > 0 {
                md.push_str("---\n\n");
            }

            // H3 heading → Docusaurus right-nav TOC entry
            md.push_str(&format!("### `{}`\n\n", sig));

            // Return type + line number on one line
            if method.range.start_line > 0 {
                md.push_str(&format!(
                    "**Returns** `{}` · **Line** :{}\n\n",
                    ret, method.range.start_line
                ));
            } else {
                md.push_str(&format!("**Returns** `{}`\n\n", ret));
            }

            // All outgoing calls as a bullet list (no truncation)
            let empty_calls: Vec<String> = Vec::new();
            let call_nodes: Vec<String> = graph
                .calls_out
                .get(method.id.as_str())
                .unwrap_or(&empty_calls)
                .iter()
                .filter_map(|cid| graph.nodes_by_id.get(cid))
                .map(callee_display)
                .collect();
            if !call_nodes.is_empty() {
                md.push_str("**Calls**\n\n");
                for call in &call_nodes {
                    md.push_str(&format!("- `{}`\n", call));
                }
                md.push('\n');
            }

            // Per-flow description blockquote (from flow LLM enrichment)
            if let Some(desc) = method_desc.get(method.id.as_str()) {
                md.push_str(&format!("> {}\n\n", desc));
            }

            // Source body block
            if !is_interface {
                if let Some(body) = bodies.get(method.id.as_str()) {
                    let location = if method.range.start_line > 0 && method.range.end_line > 0 {
                        format!("Lines {}–{}", method.range.start_line, method.range.end_line)
                    } else {
                        "Source".to_string()
                    };

                    if body.original_lines <= 80 {
                        let stripped_lines = body.stripped.trim().lines().count();
                        let code_content = if stripped_lines < body.original_lines {
                            let comment_prefix = if lang == "python" { "#" } else { "//" };
                            format!(
                                "{} stripped · {} of {} lines shown\n{}",
                                comment_prefix,
                                stripped_lines,
                                body.original_lines,
                                body.stripped.trim()
                            )
                        } else {
                            body.stripped.trim().to_string()
                        };
                        md.push_str(&format!(
                            "<details>\n<summary>{}</summary>\n\n",
                            location
                        ));
                        if lang.is_empty() {
                            md.push_str(&format!("```\n{}\n```\n\n", code_content));
                        } else {
                            md.push_str(&format!("```{}\n{}\n```\n\n", lang, code_content));
                        }
                        md.push_str("</details>\n\n");
                    } else {
                        let preview: Vec<&str> = body.stripped.lines().take(30).collect();
                        if !preview.is_empty() {
                            let comment_prefix = if lang == "python" { "#" } else { "//" };
                            let code_content = format!(
                                "{} god function · {} lines — first 30 stripped lines shown\n{}",
                                comment_prefix,
                                body.original_lines,
                                preview.join("\n")
                            );
                            md.push_str(&format!(
                                "<details>\n<summary>{} ⚠ large method</summary>\n\n",
                                location
                            ));
                            if lang.is_empty() {
                                md.push_str(&format!("```\n{}\n```\n\n", code_content));
                            } else {
                                md.push_str(&format!("```{}\n{}\n```\n\n", lang, code_content));
                            }
                            md.push_str("</details>\n\n");
                        }
                    }
                }
            }
        }
    }

    // Routes handled by methods of this class
    let class_routes: Vec<&(Node, Node)> = graph
        .routes
        .iter()
        .filter(|(handler, _)| {
            handler
                .id
                .as_str()
                .split_once('#')
                .map(|(prefix, _)| {
                    let fqcn = prefix
                        .trim_start_matches("Method:")
                        .trim_start_matches("Constructor:");
                    format!("Class:{}", fqcn) == cls_id
                })
                .unwrap_or(false)
        })
        .collect();
    if !class_routes.is_empty() {
        md.push_str("## Routes\n\n");
        md.push_str("| Method | Path | Handler |\n");
        md.push_str("|---|---|---|\n");
        for (handler, route) in &class_routes {
            md.push_str(&format!(
                "| `{}` | `{}` | `{}` |\n",
                route_http_method(route),
                route_path(route),
                handler.name
            ));
        }
        md.push('\n');
    }

    // DB access aggregated from all methods
    let mut raw_db: std::collections::BTreeMap<String, (bool, bool)> =
        std::collections::BTreeMap::new();
    for method in all_methods {
        if let Some(query_ids) = graph.executes_query.get(method.id.as_str()) {
            for qid in query_ids {
                for tid in graph
                    .query_reads_table
                    .get(qid.as_str())
                    .into_iter()
                    .flatten()
                {
                    let name = tid.strip_prefix("DbTable:").unwrap_or(tid).to_string();
                    raw_db.entry(name).or_default().0 = true;
                }
                for tid in graph
                    .query_writes_table
                    .get(qid.as_str())
                    .into_iter()
                    .flatten()
                {
                    let name = tid.strip_prefix("DbTable:").unwrap_or(tid).to_string();
                    raw_db.entry(name).or_default().1 = true;
                }
            }
        }
    }
    if !raw_db.is_empty() {
        md.push_str("## DB Access\n\n");
        md.push_str("| Table | Read | Write |\n");
        md.push_str("|---|---|---|\n");
        for (table, (r, w)) in &raw_db {
            md.push_str(&format!(
                "| `{}` | {} | {} |\n",
                table,
                if *r { "✓" } else { "" },
                if *w { "✓" } else { "" }
            ));
        }
        md.push('\n');
    }

    // External calls
    let mut ext_call_names: Vec<String> = Vec::new();
    for method in all_methods {
        if let Some(ext_ids) = graph.external_calls.get(method.id.as_str()) {
            for eid in ext_ids {
                if let Some(ext_node) = graph.nodes_by_id.get(eid) {
                    if !ext_call_names.contains(&ext_node.name) {
                        ext_call_names.push(ext_node.name.clone());
                    }
                }
            }
        }
    }
    if !ext_call_names.is_empty() {
        md.push_str("## External Calls\n\n");
        for name in &ext_call_names {
            md.push_str(&format!("- `{}`\n", name));
        }
        md.push('\n');
    }

    // Tests: find test classes that test any method of this class via Tests edges
    let mut test_class_names: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for method in all_methods {
        if let Some(tester_ids) = graph.tests_in.get(method.id.as_str()) {
            for tid in tester_ids {
                if let Some(tcls_id) = tid.split_once('#').map(|(prefix, _)| {
                    format!("Class:{}", prefix.trim_start_matches("Method:"))
                }) {
                    if let Some(tcls_node) = graph.nodes_by_id.get(&tcls_id) {
                        test_class_names.insert(tcls_node.name.clone());
                    }
                }
            }
        }
    }
    // Also check tests_in at the class level
    if let Some(tester_ids) = graph.tests_in.get(cls_id) {
        for tid in tester_ids {
            if let Some(tcls_node) = graph.nodes_by_id.get(tid) {
                test_class_names.insert(tcls_node.name.clone());
            }
        }
    }
    if !test_class_names.is_empty() {
        md.push_str("## Tests\n\n");
        for name in &test_class_names {
            md.push_str(&format!("- `{}`\n", name));
        }
        md.push('\n');
    }

    // Important files
    let mut files: Vec<&str> = all_methods
        .iter()
        .filter(|n| !n.file.is_empty())
        .map(|n| n.file.as_str())
        .collect();
    if !cls.file.is_empty() {
        files.push(cls.file.as_str());
    }
    files.sort_unstable();
    files.dedup();

    if !files.is_empty() {
        md.push_str("## Important Files\n\n");
        for f in files.iter().take(10) {
            md.push_str(&format!("- `{}`\n", f));
        }
        if files.len() > 10 {
            md.push_str(&format!("- _…and {} more_\n", files.len() - 10));
        }
        md.push('\n');
    }

    md
}

/// JSON representation of a class page for search/AI use.
pub fn render_dev_class_json(graph: &WikiGraph, cls: &Node) -> serde_json::Value {
    let cls_id = cls.id.as_str();

    let empty_methods: Vec<Node> = Vec::new();
    let methods = graph.methods_by_class.get(cls_id).unwrap_or(&empty_methods);

    // Collect this class + all direct call neighbors
    let mut neighbor_ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    neighbor_ids.insert(cls_id.to_string());
    let mut seen_edges: Vec<(String, String)> = Vec::new();

    for method in methods {
        if let Some(callees) = graph.calls_out.get(method.id.as_str()) {
            for callee in callees {
                if let Some(callee_class) = callee.split_once('#').map(|(prefix, _)| {
                    format!(
                        "Class:{}",
                        prefix
                            .trim_start_matches("Method:")
                            .trim_start_matches("Constructor:")
                    )
                }) {
                    if callee_class != cls_id {
                        let edge = (cls_id.to_string(), callee_class.clone());
                        if !seen_edges.contains(&edge) {
                            seen_edges.push(edge);
                            neighbor_ids.insert(callee_class);
                        }
                    }
                }
            }
        }
        if let Some(callers) = graph.calls_in.get(method.id.as_str()) {
            for caller in callers {
                if let Some(caller_class) = caller.split_once('#').map(|(prefix, _)| {
                    format!(
                        "Class:{}",
                        prefix
                            .trim_start_matches("Method:")
                            .trim_start_matches("Constructor:")
                    )
                }) {
                    if caller_class != cls_id {
                        let edge = (caller_class.clone(), cls_id.to_string());
                        if !seen_edges.contains(&edge) {
                            seen_edges.push(edge);
                            neighbor_ids.insert(caller_class);
                        }
                    }
                }
            }
        }
    }

    let nodes: Vec<serde_json::Value> = neighbor_ids
        .iter()
        .filter_map(|cid| graph.nodes_by_id.get(cid))
        .map(|n| {
            serde_json::json!({
                "id": n.id.as_str(),
                "label": n.name.as_str(),
                "kind": n.kind.label(),
                "stereotype": node_stereotype(n).unwrap_or(""),
            })
        })
        .collect();

    let links: Vec<serde_json::Value> = seen_edges
        .into_iter()
        .filter(|(src, dst)| neighbor_ids.contains(src.as_str()) && neighbor_ids.contains(dst.as_str()))
        .map(|(src, dst)| {
            serde_json::json!({
                "source": src,
                "target": dst,
                "label": "CALLS",
            })
        })
        .collect();

    serde_json::json!({
        "format": "d3-force",
        "class_id": cls_id,
        "nodes": nodes,
        "links": links,
    })
}

pub fn render_dev_community_json(graph: &WikiGraph, community: &Node) -> serde_json::Value {
    let comm_id = community.id.as_str();
    let empty_members: Vec<Node> = Vec::new();
    let member_list = graph
        .members_by_community
        .get(comm_id)
        .unwrap_or(&empty_members);

    let classes: Vec<&Node> = member_list
        .iter()
        .filter(|n| {
            matches!(
                n.kind,
                NodeKind::Class
                    | NodeKind::Interface
                    | NodeKind::Enum
                    | NodeKind::Record
                    | NodeKind::Annotation
            )
        })
        .collect();

    let nodes: Vec<serde_json::Value> = classes
        .iter()
        .map(|n| {
            serde_json::json!({
                "id": n.id.as_str(),
                "label": n.name.as_str(),
                "kind": n.kind.label(),
                "stereotype": node_stereotype(n).unwrap_or(""),
            })
        })
        .collect();

    let class_ids: std::collections::HashSet<String> =
        classes.iter().map(|n| n.id.as_str().to_string()).collect();

    let links: Vec<serde_json::Value> = classes
        .iter()
        .flat_map(|cls| {
            let src_id = cls.id.as_str().to_string();
            let empty: Vec<String> = Vec::new();
            let dsts = graph.calls_out.get(&src_id).unwrap_or(&empty);
            dsts.iter()
                .filter(|d| class_ids.contains(*d))
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
        "format": "d3-force",
        "community_id": comm_id,
        "nodes": nodes,
        "links": links,
    })
}



