use std::collections::{BTreeMap, HashMap};

use crate::graph::WikiGraph;
use crate::mermaid;
use crate::{capitalize, CommunityLlmFull, CommunityLlmSummary, FeatureLlmSummary, FlowLlmSummary};


/// Render the feature-level BA (business analysis) page.
/// Aggregates workflows, cross-module calls, and LLM summaries.
pub fn render_feature_ba(
    feature: &str,
    community_ids: &[String],
    graph: &WikiGraph,
    llm_summaries: Option<&HashMap<String, CommunityLlmSummary>>,
    llm_full: Option<&HashMap<String, CommunityLlmFull>>,
    feature_llm: Option<&FeatureLlmSummary>,
    flow_llm_summaries: Option<&HashMap<String, FlowLlmSummary>>,
) -> String {
    let title = format!("{} — Business Analysis", capitalize(feature));
    let mut md = String::new();
    md.push_str(&format!("---\ntitle: {}\nsidebar_position: 2\n---\n\n", title));
    md.push_str(&format!("# {}\n\n", title));

    // Mermaid process flow diagram (business flows only)
    if let Some(diagram) = mermaid::process_flow_diagram(graph, community_ids, true) {
        md.push_str("## Process Diagram\n\n");
        md.push_str("```mermaid\n");
        md.push_str(&diagram);
        md.push_str("```\n\n");
    }

    // Feature-level LLM overview (highest quality — one call across all communities).
    if let Some(flm) = feature_llm {
        if !flm.ba_process_overview.is_empty() {
            md.push_str("## Process Overview\n\n");
            md.push_str(&flm.ba_process_overview);
            md.push_str("\n\n");
        }
        if !flm.ba_business_rules.is_empty() {
            md.push_str("## Business Rules\n\n");
            md.push_str(&flm.ba_business_rules);
            md.push_str("\n\n");
        }
    } else {
        // llm-full mode: richer sections per community
        let full_entries: Vec<&CommunityLlmFull> = community_ids
            .iter()
            .filter_map(|cid| llm_full.and_then(|m| m.get(cid)))
            .collect();

        if !full_entries.is_empty() {
            let overviews: Vec<&str> = full_entries
                .iter()
                .map(|f| f.ba_process_overview.as_str())
                .filter(|s| !s.is_empty())
                .collect();
            if !overviews.is_empty() {
                md.push_str("## Process Overview\n\n");
                for s in &overviews {
                    md.push_str(s);
                    md.push_str("\n\n");
                }
            }
            let contracts: Vec<&str> = full_entries
                .iter()
                .map(|f| f.ba_contracts.as_str())
                .filter(|s| !s.is_empty())
                .collect();
            if !contracts.is_empty() {
                md.push_str("## Contracts\n\n");
                for s in &contracts {
                    md.push_str(s);
                    md.push_str("\n\n");
                }
            }
            let rules: Vec<&str> = full_entries
                .iter()
                .map(|f| f.ba_business_rules.as_str())
                .filter(|s| !s.is_empty())
                .collect();
            if !rules.is_empty() {
                md.push_str("## Business Rules\n\n");
                for s in &rules {
                    md.push_str(s);
                    md.push_str("\n\n");
                }
            }
        } else {
            // llm-summary mode fallback
            let ba_texts: Vec<String> = community_ids
                .iter()
                .filter_map(|cid| llm_summaries.and_then(|m| m.get(cid)).map(|s| s.ba.clone()))
                .filter(|s| !s.is_empty())
                .collect();

            if !ba_texts.is_empty() {
                md.push_str("## Process Overview\n\n");
                for text in &ba_texts {
                    md.push_str(text);
                    md.push_str("\n\n");
                }
            }
        }
    } // end feature_llm / community-level LLM

    // Per-community workflow sections (business flows only)
    let mut any_workflows = false;
    for cid in community_ids {
        let procs = graph.processes_for_community(cid, true);
        if procs.is_empty() {
            continue;
        }
        if !any_workflows {
            md.push_str("## Workflows\n\n");
            any_workflows = true;
        }
        let comm_name = graph.community_display_name(cid);
        md.push_str(&format!("### {}\n\n", comm_name));

        for proc_id in &procs {
            if let Some(proc_node) = graph.nodes_by_id.get(proc_id) {
                md.push_str(&format!("#### {}\n\n", proc_node.name));
                let flow_summary = flow_llm_summaries.and_then(|m| m.get(proc_id.as_str()));
                if let Some(steps) = graph.process_steps.get(proc_id.as_str()) {
                    if let Some(fs) = flow_summary {
                        if !fs.narrative.is_empty() {
                            md.push_str(&format!("*{}*\n\n", fs.narrative));
                        }
                        md.push_str("| Step | Method | What it does |\n");
                        md.push_str("|---|---|---|\n");
                        for (i, step) in steps.iter().enumerate() {
                            let desc = fs.step_descriptions.get(i).map(|s| s.as_str()).unwrap_or("");
                            md.push_str(&format!("| {} | `{}` | {} |\n", i + 1, step.symbol.name, desc));
                        }
                        md.push('\n');
                    } else {
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
    }

    // Cross-module dependencies involving this feature's communities
    let feature_set: std::collections::HashSet<&str> =
        community_ids.iter().map(|s| s.as_str()).collect();
    let deps: Vec<(String, String, usize)> = graph
        .inter_community_calls
        .iter()
        .filter(|(src, dst, _)| {
            feature_set.contains(src.as_str()) || feature_set.contains(dst.as_str())
        })
        .map(|(src, dst, count)| {
            (
                graph.community_name(src).to_string(),
                graph.community_name(dst).to_string(),
                *count,
            )
        })
        .collect();

    if !deps.is_empty() {
        md.push_str("## Dependencies\n\n");
        md.push_str("| Caller | Callee | Calls |\n");
        md.push_str("|---|---|---|\n");
        for (src, dst, count) in &deps {
            md.push_str(&format!("| {} | {} | {} |\n", src, dst, count));
        }
        md.push('\n');
    }

    // Messaging topics
    let mut publishes: BTreeMap<String, String> = BTreeMap::new();
    let mut consumes: BTreeMap<String, String> = BTreeMap::new();
    for cid in community_ids {
        let (pub_topics, con_topics) = graph.community_messaging(cid);
        for (name, kind) in pub_topics {
            publishes.insert(name, kind);
        }
        for (name, kind) in con_topics {
            consumes.insert(name, kind);
        }
    }
    if !publishes.is_empty() || !consumes.is_empty() {
        md.push_str("## Topics\n\n");
        md.push_str("| Direction | Topic | Type |\n");
        md.push_str("|---|---|---|\n");
        for (name, kind) in &publishes {
            md.push_str(&format!(
                "| Publishes | `{}` | {} |\n",
                name,
                capitalize(&kind)
            ));
        }
        for (name, kind) in &consumes {
            md.push_str(&format!(
                "| Consumes | `{}` | {} |\n",
                name,
                capitalize(&kind)
            ));
        }
        md.push('\n');
    }

    // Aggregated data access
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
        md.push_str("## Data Access\n\n");
        md.push_str("| Table | Read | Write |\n");
        md.push_str("|---|---|---|\n");
        for (name, (reads, writes)) in &tables {
            md.push_str(&format!(
                "| `{}` | {} | {} |\n",
                name,
                if *reads { "✓" } else { "" },
                if *writes { "✓" } else { "" },
            ));
        }
        md.push('\n');
    }

    md
}



