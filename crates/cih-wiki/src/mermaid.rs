use crate::graph::WikiGraph;

const MAX_NODES: usize = 20;
const MAX_EDGES: usize = 30;
const MAX_SEQ_PARTICIPANTS: usize = 8;

/// Unescape common HTML entities so that graph-stored names like `&lt;init&gt;`
/// are treated as raw `<init>` before Mermaid-specific escaping is applied.
fn unescape_html(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// Escape a label for use inside a Mermaid `["..."]` flowchart node.
/// Angle brackets become parens — Mermaid's parser rejects `&lt;`/`&gt;` in node labels.
/// HTML entities from the graph are unescaped first so `&lt;init&gt;` → `(init)`.
#[doc(hidden)]
pub fn sanitize(s: &str) -> String {
    let s = unescape_html(s);
    s.replace('"', "&quot;")
        .replace('<', "(")
        .replace('>', ")")
        .replace('\n', " ")
        // double-dash is parsed as an arrow; replace with em-dash
        .replace("--", "—")
}

/// Escape a label for use in a Mermaid `sequenceDiagram` message.
/// Mermaid's sequenceDiagram parser rejects HTML entities in message text,
/// so angle brackets must become plain parens rather than &lt;/&gt;.
/// HTML entities from the graph are unescaped first so `&lt;init&gt;` → `(init)`.
fn sanitize_seq(s: &str) -> String {
    let s = unescape_html(s);
    s.replace('<', "(")
        .replace('>', ")")
        .replace('\n', " ")
        .replace(':', ";")
        .replace("--", "—")
}

fn node_id(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Generate a `flowchart LR` process-step diagram for a feature's communities.
/// When `business_only` is true, only includes processes with `business_flow == true`.
/// Returns `None` if there are fewer than 2 connected steps.
pub fn process_flow_diagram(
    graph: &WikiGraph,
    community_ids: &[String],
    business_only: bool,
) -> Option<String> {
    let mut steps: Vec<(String, String)> = Vec::new(); // (id, label)
    let mut arrows: Vec<(String, String)> = Vec::new(); // (from_id, to_id)

    for comm_id in community_ids {
        let Some(members) = graph.members_by_community.get(comm_id) else {
            continue;
        };
        for member in members {
            let mid = member.id.as_str();
            if let Some(proc_list) = find_processes_for_member(graph, mid) {
                for proc_id in proc_list {
                    if business_only && !graph.is_business_process(proc_id) {
                        continue;
                    }
                    if let Some(proc_steps) = graph.process_steps.get(proc_id.as_str()) {
                        for pair in proc_steps.windows(2) {
                            let from_label = sanitize(&pair[0].symbol.name);
                            let to_label = sanitize(&pair[1].symbol.name);
                            let from_nid =
                                node_id(&pair[0].symbol.id.as_str().replace("Method:", ""));
                            let to_nid =
                                node_id(&pair[1].symbol.id.as_str().replace("Method:", ""));
                            if !steps.iter().any(|(id, _)| id == &from_nid) {
                                steps.push((from_nid.clone(), from_label));
                            }
                            if !steps.iter().any(|(id, _)| id == &to_nid) {
                                steps.push((to_nid.clone(), to_label));
                            }
                            if !arrows.contains(&(from_nid.clone(), to_nid.clone())) {
                                arrows.push((from_nid, to_nid));
                            }
                            if steps.len() >= MAX_NODES || arrows.len() >= MAX_EDGES {
                                break;
                            }
                        }
                    }
                    if steps.len() >= MAX_NODES {
                        break;
                    }
                }
            }
        }
    }

    if steps.len() < 2 || arrows.is_empty() {
        return None;
    }

    let truncated = steps.len() >= MAX_NODES || arrows.len() >= MAX_EDGES;
    let mut out = String::from("flowchart LR\n");
    for (id, label) in &steps {
        out.push_str(&format!("  {}[\"{}\"]\n", id, label));
    }
    for (from, to) in &arrows {
        out.push_str(&format!("  {} --> {}\n", from, to));
    }
    if truncated {
        out.push_str("  %%diagram truncated\n");
    }
    Some(out)
}

fn find_processes_for_member<'a>(graph: &'a WikiGraph, member_id: &str) -> Option<Vec<&'a String>> {
    let result: Vec<&'a String> = graph
        .process_steps
        .iter()
        .filter_map(|(proc_id, steps)| {
            if steps.iter().any(|s| s.symbol.id.as_str() == member_id) {
                Some(proc_id)
            } else {
                None
            }
        })
        .collect();
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// Generate a `flowchart LR` class-level call diagram for a community's dev page.
/// Shows classes inside the community and the external classes/services they call.
/// This is more useful than community_call_diagram for tightly-coupled communities
/// (e.g. controller+service in the same Louvain cluster) because it operates on
/// class-to-class edges regardless of community boundaries.
pub fn class_call_diagram(graph: &WikiGraph, comm_id: &str) -> Option<String> {
    let members = graph.members_by_community.get(comm_id)?;

    // Derive home class IDs from member method IDs
    let home_class_ids: std::collections::HashSet<String> = members
        .iter()
        .filter_map(|m| class_id_of(m.id.as_str()))
        .collect();

    if home_class_ids.is_empty() {
        return None;
    }

    // Build class-to-class call edges:
    // for each method in the community, find callee methods and their classes
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut all_class_ids: std::collections::HashSet<String> = home_class_ids.clone();

    for member in members {
        let Some(caller_class) = class_id_of(member.id.as_str()) else {
            continue;
        };
        let Some(callees) = graph.calls_out.get(member.id.as_str()) else {
            continue;
        };
        for callee in callees {
            let Some(callee_class) = class_id_of(callee) else {
                continue;
            };
            if callee_class == caller_class {
                continue; // skip intra-class calls
            }
            let edge = (caller_class.clone(), callee_class.clone());
            if !edges.contains(&edge) {
                edges.push(edge);
                all_class_ids.insert(callee_class);
            }
            if edges.len() >= MAX_EDGES {
                break;
            }
        }
        if edges.len() >= MAX_EDGES {
            break;
        }
    }

    if edges.is_empty() {
        return None;
    }

    // Filter: only include nodes reachable through an edge
    let mut seen_nodes: Vec<String> = Vec::new();
    for (src, dst) in &edges {
        if !seen_nodes.contains(src) {
            seen_nodes.push(src.clone());
        }
        if !seen_nodes.contains(dst) {
            seen_nodes.push(dst.clone());
        }
        if seen_nodes.len() >= MAX_NODES {
            break;
        }
    }

    let class_label = |cid: &str| -> String {
        let simple = cid
            .trim_start_matches("Class:")
            .rsplit('.')
            .next()
            .unwrap_or(cid)
            .to_string();
        // look up stereotype from the Class node
        let stereo = graph
            .nodes_by_id
            .get(cid)
            .and_then(|n| n.props.as_ref())
            .and_then(|p| p.get("stereotype"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        match stereo {
            Some(s) => format!("{} ({})", simple, s),
            None => simple,
        }
    };

    let truncated = edges.len() >= MAX_EDGES || seen_nodes.len() >= MAX_NODES;
    let mut out = String::from("flowchart LR\n");
    for cid in &seen_nodes {
        let label = sanitize(&class_label(cid));
        let nid = node_id(cid);
        out.push_str(&format!("  {}[\"{}\"]\n", nid, label));
    }
    for (src, dst) in &edges {
        if seen_nodes.contains(src) && seen_nodes.contains(dst) {
            out.push_str(&format!("  {} --> {}\n", node_id(src), node_id(dst)));
        }
    }
    if truncated {
        out.push_str("  %%diagram truncated\n");
    }
    Some(out)
}

/// Generate a `flowchart LR` class-level call diagram for a class's dev page.
/// Shows the class with its callers (inbound) and callees (outbound).
pub fn class_call_diagram_for_class(graph: &WikiGraph, class_id: &str) -> Option<String> {
    let methods = graph.methods_by_class.get(class_id)?;

    let mut edges: Vec<(String, String)> = Vec::new();
    let mut seen_nodes: Vec<String> = Vec::new();
    seen_nodes.push(class_id.to_string());

    // Outgoing: this class calls others
    'outer_out: for method in methods {
        if let Some(callees) = graph.calls_out.get(method.id.as_str()) {
            for callee in callees {
                if let Some(callee_class) = class_id_of(callee) {
                    if callee_class == class_id {
                        continue;
                    }
                    let edge = (class_id.to_string(), callee_class.clone());
                    if !edges.contains(&edge) {
                        edges.push(edge);
                        if !seen_nodes.contains(&callee_class) {
                            seen_nodes.push(callee_class);
                        }
                    }
                    if edges.len() >= MAX_EDGES {
                        break 'outer_out;
                    }
                }
            }
        }
    }

    // Incoming: other classes call into this class
    'outer_in: for method in methods {
        if let Some(callers) = graph.calls_in.get(method.id.as_str()) {
            for caller in callers {
                if let Some(caller_class) = class_id_of(caller) {
                    if caller_class == class_id {
                        continue;
                    }
                    let edge = (caller_class.clone(), class_id.to_string());
                    if !edges.contains(&edge) {
                        edges.push(edge);
                        if !seen_nodes.contains(&caller_class) {
                            seen_nodes.push(caller_class);
                        }
                    }
                    if edges.len() >= MAX_EDGES {
                        break 'outer_in;
                    }
                }
                if seen_nodes.len() >= MAX_NODES {
                    break 'outer_in;
                }
            }
        }
    }

    if edges.is_empty() {
        return None;
    }

    // Truncate seen_nodes to MAX_NODES
    seen_nodes.truncate(MAX_NODES);

    let class_label = |cid: &str| -> String {
        let simple = cid
            .trim_start_matches("Class:")
            .rsplit('.')
            .next()
            .unwrap_or(cid)
            .to_string();
        let stereo = graph
            .nodes_by_id
            .get(cid)
            .and_then(|n| n.props.as_ref())
            .and_then(|p| p.get("stereotype"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        match stereo {
            Some(s) => format!("{} ({})", simple, s),
            None => simple,
        }
    };

    let truncated = edges.len() >= MAX_EDGES || seen_nodes.len() >= MAX_NODES;
    let mut out = String::from("flowchart LR\n");
    for cid in &seen_nodes {
        let label = sanitize(&class_label(cid));
        let nid = node_id(cid);
        out.push_str(&format!("  {}[\"{}\"]\n", nid, label));
    }
    for (src, dst) in &edges {
        if seen_nodes.contains(src) && seen_nodes.contains(dst) {
            out.push_str(&format!("  {} --> {}\n", node_id(src), node_id(dst)));
        }
    }
    if truncated {
        out.push_str("  %%diagram truncated\n");
    }
    Some(out)
}

fn class_id_of(method_id: &str) -> Option<String> {
    let stripped = method_id
        .trim_start_matches("Method:")
        .trim_start_matches("Constructor:")
        .trim_start_matches("Function:");
    // "pkg.ClassName#methodName/arity" → "Class:pkg.ClassName"
    stripped
        .split_once('#')
        .map(|(fqcn, _)| format!("Class:{}", fqcn))
}

/// Generate a `flowchart LR` diagram showing how communities call each other,
/// filtered to calls involving `comm_id`.
pub fn community_call_diagram(graph: &WikiGraph, comm_id: &str) -> Option<String> {
    let relevant: Vec<&(String, String, usize)> = graph
        .inter_community_calls
        .iter()
        .filter(|(src, dst, _)| src == comm_id || dst == comm_id)
        .collect();

    if relevant.is_empty() {
        return None;
    }

    // Collect unique community IDs involved
    let mut comm_ids: Vec<&str> = Vec::new();
    for (src, dst, _) in &relevant {
        if !comm_ids.contains(&src.as_str()) {
            comm_ids.push(src);
        }
        if !comm_ids.contains(&dst.as_str()) {
            comm_ids.push(dst);
        }
        if comm_ids.len() >= MAX_NODES {
            break;
        }
    }

    // Compute a base label per community: "DisplayName (stereotype)"
    let base_label_of = |cid: &str| -> String {
        let display = graph.community_display_name(cid);
        let stereotype = graph
            .nodes_by_id
            .get(cid)
            .and_then(|n| n.props.as_ref())
            .and_then(|p| p.get("primary_stereotype"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        match stereotype {
            Some(s) => format!("{} ({})", display, s),
            None => display.to_string(),
        }
    };

    // Merge communities that share the same base label into one canonical ID.
    // This collapses Louvain split-class artifacts (e.g. two "Cart (controller)" clusters).
    let mut label_to_canonical: std::collections::HashMap<String, &str> =
        std::collections::HashMap::new();
    let mut canonical_for: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for &cid in &comm_ids {
        let lbl = base_label_of(cid);
        let canon = *label_to_canonical.entry(lbl).or_insert(cid);
        canonical_for.insert(cid, canon);
    }

    // Deduplicated node list (canonical IDs only)
    let mut seen_nodes: Vec<&str> = Vec::new();
    for &cid in &comm_ids {
        let canon = canonical_for[cid];
        if !seen_nodes.contains(&canon) {
            seen_nodes.push(canon);
        }
    }

    if seen_nodes.len() < 2 {
        return None;
    }

    let truncated = comm_ids.len() >= MAX_NODES || relevant.len() >= MAX_EDGES;

    let mut out = String::from("flowchart LR\n");
    for &cid in &seen_nodes {
        let label = sanitize(&base_label_of(cid));
        let nid = node_id(cid);
        out.push_str(&format!("  {}[\"{}\"]\n", nid, label));
    }

    // Emit edges using canonical IDs; drop self-loops produced by merging
    let mut seen_edges: Vec<(String, String)> = Vec::new();
    for (src, dst, count) in relevant.iter().take(MAX_EDGES) {
        let csrc = canonical_for
            .get(src.as_str())
            .copied()
            .unwrap_or(src.as_str());
        let cdst = canonical_for
            .get(dst.as_str())
            .copied()
            .unwrap_or(dst.as_str());
        if csrc == cdst {
            continue; // merged — intra-class call, skip
        }
        let edge_key = (csrc.to_string(), cdst.to_string());
        if seen_edges.contains(&edge_key) {
            continue;
        }
        seen_edges.push(edge_key);
        let arrow = if *count > 1 {
            format!(" --\"{}x\"--> ", count)
        } else {
            " --> ".to_string()
        };
        out.push_str(&format!("  {}{}{}\n", node_id(csrc), arrow, node_id(cdst)));
    }
    if truncated {
        out.push_str("  %%diagram truncated\n");
    }
    Some(out)
}

/// Build a `sequenceDiagram` block from a BFS call chain plus the raw `calls_out` graph.
///
/// Uses the actual call edges to determine which class calls which, so the arrows
/// reflect real control flow rather than BFS visit order.  Participants are listed
/// in BFS first-appearance order.  Returns `None` when fewer than two distinct
/// classes or no cross-class edges exist.
///
/// `http_method` / `path` are passed for HTTP entry points so a "Client" actor
/// and the initial request arrow can be rendered; pass empty strings for
/// scheduled / listener entry points.
pub fn call_sequence_diagram(
    chain: &[String],
    calls_out: &std::collections::BTreeMap<String, Vec<String>>,
    http_method: &str,
    path: &str,
) -> Option<String> {
    fn cls_name(method_id: &str) -> String {
        let stripped = method_id
            .strip_prefix("Method:")
            .or_else(|| method_id.strip_prefix("Constructor:"))
            .or_else(|| method_id.strip_prefix("Function:"))
            .unwrap_or(method_id);
        let fqcn = stripped.split('#').next().unwrap_or(stripped);
        fqcn.rsplit('.').next().unwrap_or(fqcn).to_string()
    }

    fn meth_name(method_id: &str) -> &str {
        method_id
            .split('#')
            .nth(1)
            .and_then(|s| s.split('/').next())
            .unwrap_or(method_id)
    }

    let chain_set: std::collections::HashSet<&str> = chain.iter().map(|s| s.as_str()).collect();

    // Unique participants in BFS first-appearance order.
    let mut participants: Vec<String> = Vec::new();
    for mid in chain {
        let cls = cls_name(mid);
        if !participants.contains(&cls) {
            participants.push(cls);
            if participants.len() >= MAX_SEQ_PARTICIPANTS {
                break;
            }
        }
    }

    if participants.len() < 2 {
        return None;
    }

    // Build class-level arrows using actual call edges.
    // For each method in the chain, scan its callees that are also in the chain.
    // If the callee is in a different class, record the (caller_class, callee_class) pair.
    let mut arrows: Vec<(String, String, String)> = Vec::new(); // (from, to, first_callee_method)
    for mid in chain {
        let caller_cls = cls_name(mid);
        let Some(callees) = calls_out.get(mid.as_str()) else {
            continue;
        };
        for callee_id in callees {
            if !chain_set.contains(callee_id.as_str()) {
                continue;
            }
            let callee_cls = cls_name(callee_id);
            if callee_cls == caller_cls {
                continue; // intra-class call — skip
            }
            // Only add arrow if both classes are known participants.
            if !participants.contains(&caller_cls) || !participants.contains(&callee_cls) {
                continue;
            }
            if !arrows
                .iter()
                .any(|(f, t, _)| f == &caller_cls && t == &callee_cls)
            {
                let label = meth_name(callee_id).to_string();
                arrows.push((caller_cls.clone(), callee_cls, label));
            }
        }
    }

    if arrows.is_empty() {
        return None;
    }

    let has_http = !http_method.is_empty();
    let truncated = participants.len() >= MAX_SEQ_PARTICIPANTS;

    let mut out = String::from("sequenceDiagram\n");
    if has_http {
        out.push_str("    actor Client\n");
    }
    for p in &participants {
        out.push_str(&format!("    participant {}\n", p));
    }
    if has_http && !participants.is_empty() {
        out.push_str(&format!(
            "    Client->>{}: {} {}\n",
            participants[0],
            http_method,
            sanitize_seq(path)
        ));
    }
    for (from, to, label) in &arrows {
        out.push_str(&format!("    {}->>{}:{}\n", from, to, sanitize_seq(label)));
    }
    if truncated {
        if let Some(first_p) = participants.first() {
            out.push_str(&format!("    Note over {}: …truncated\n", first_p));
        }
    }
    Some(out)
}
