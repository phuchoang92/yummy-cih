use std::collections::HashMap;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::llm::{LlmAdapter, LlmRequest};
use cih_wiki::{ModuleTreeSource, WikiGraph, WikiModuleNode, WikiModuleTree};

const MAX_EVIDENCE_CHARS: usize = 20_000;

// ── Shared response types ────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug)]
struct GroupingResponse {
    modules: Vec<ModuleProposal>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ModuleProposal {
    pub slug: String,
    pub title: String,
    pub description: String,
    pub community_ids: Vec<String>,
}

// Phase 1: LLM proposes module outline only (no community assignments yet)
#[derive(Serialize, Deserialize, Debug)]
struct OutlineResponse {
    modules: Vec<OutlineModule>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OutlineModule {
    pub slug: String,
    pub title: String,
    pub description: String,
}

// ── Public entry point ───────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)] // LLM-enrichment context bundle; refactor tracked with wiki rework
pub fn propose_module_tree(
    graph: &WikiGraph,
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    graph_version: &str,
    community_version: &str,
) -> Result<WikiModuleTree> {
    let evidence = build_detailed_evidence(graph);
    let estimated_modules = estimate_module_count(graph);

    let proposals = if evidence.len() <= MAX_EVIDENCE_CHARS {
        // Single-shot: all communities fit in one call
        call_grouping_llm(
            adapter,
            api_key,
            model,
            max_tokens,
            timeout_secs,
            &evidence,
            estimated_modules,
        )?
    } else {
        // Two-phase: outline first, then batch-assign
        two_phase_grouping(
            graph,
            adapter,
            api_key,
            model,
            max_tokens,
            timeout_secs,
            &evidence,
            estimated_modules,
        )?
    };

    proposals_to_tree(proposals, graph, graph_version, community_version)
}

// ── Evidence builders ────────────────────────────────────────────────────────

/// Full per-community evidence line, enriched with route_prefixes, controllers,
/// db_tables, and the path-heuristic feature hint.
fn build_detailed_evidence(graph: &WikiGraph) -> String {
    graph
        .community_nodes
        .iter()
        .map(|comm| {
            let comm_id = comm.id.as_str();
            let props = comm.props.as_ref();

            let hint = props
                .and_then(|p| p.get("feature"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let stereotype = props
                .and_then(|p| p.get("primary_stereotype"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let route_count = graph
                .community_routes
                .get(comm_id)
                .map(|r| r.len())
                .unwrap_or(0);

            let prefixes: Vec<&str> = props
                .and_then(|p| p.get("route_prefixes"))
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();

            let controllers: Vec<&str> = props
                .and_then(|p| p.get("controllers"))
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).take(3).collect())
                .unwrap_or_default();

            let tables: Vec<&str> = props
                .and_then(|p| p.get("db_tables"))
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).take(5).collect())
                .unwrap_or_default();

            let flows: Vec<&str> = graph
                .process_nodes
                .iter()
                .filter(|p| {
                    p.props
                        .as_ref()
                        .and_then(|v| v.get("communities"))
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter().any(|c| c.as_str() == Some(comm_id)))
                        .unwrap_or(false)
                })
                .filter_map(|p| {
                    p.props
                        .as_ref()
                        .and_then(|v| v.get("label"))
                        .and_then(|v| v.as_str())
                })
                .take(3)
                .collect();

            let mut parts = vec![
                format!("id={comm_id}"),
                format!("hint={hint}"),
                format!("stereotype={stereotype}"),
                format!("routes={route_count}"),
            ];
            if !prefixes.is_empty() {
                parts.push(format!("prefixes=[{}]", prefixes.join(",")));
            }
            if !controllers.is_empty() {
                parts.push(format!("controllers=[{}]", controllers.join(",")));
            }
            if !tables.is_empty() {
                parts.push(format!("tables=[{}]", tables.join(",")));
            }
            if !flows.is_empty() {
                parts.push(format!("flows=[{}]", flows.join(",")));
            }

            format!("- {} ({})", comm.name, parts.join(", "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Compressed summary for Phase 1 outline call: aggregates feature hints
/// with community counts and lists all distinct route prefixes.
fn build_outline_evidence(graph: &WikiGraph) -> String {
    let mut feature_counts: HashMap<&str, usize> = HashMap::new();
    let mut all_prefixes: Vec<&str> = Vec::new();

    for comm in &graph.community_nodes {
        let props = comm.props.as_ref();
        let hint = props
            .and_then(|p| p.get("feature"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        *feature_counts.entry(hint).or_insert(0) += 1;

        if let Some(prefixes) = props
            .and_then(|p| p.get("route_prefixes"))
            .and_then(|v| v.as_array())
        {
            for v in prefixes {
                if let Some(s) = v.as_str() {
                    all_prefixes.push(s);
                }
            }
        }
    }

    all_prefixes.sort_unstable();
    all_prefixes.dedup();

    let mut feature_lines: Vec<String> = feature_counts
        .iter()
        .map(|(f, c)| format!("  {f} ({c} communities)"))
        .collect();
    feature_lines.sort();

    format!(
        "Total communities: {total}\n\
         \n\
         Auto-detected feature hints (improve/merge these):\n\
         {features}\n\
         \n\
         All route prefixes found: {prefixes}",
        total = graph.community_nodes.len(),
        features = feature_lines.join("\n"),
        prefixes = all_prefixes.join(", "),
    )
}

// ── LLM callers ─────────────────────────────────────────────────────────────

fn call_grouping_llm(
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    evidence: &str,
    estimated_modules: usize,
) -> Result<Vec<ModuleProposal>> {
    let req = LlmRequest {
        system: build_grouping_system(estimated_modules),
        user: build_grouping_user(evidence),
        model: model.to_string(),
        max_tokens,
        timeout_secs,
    };
    let resp = adapter.call(api_key, &req)?;
    parse_grouping_response(&resp.text)
}

fn call_outline_llm(
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    outline_evidence: &str,
    estimated_modules: usize,
) -> Result<Vec<OutlineModule>> {
    let system = crate::llm::prompts::MODULE_OUTLINE_SYSTEM_PROMPT
        .replace("{estimated_modules}", &estimated_modules.to_string());
    let user = format!(
        "Based on this codebase summary, propose a module outline:\n\n\
         {outline_evidence}\n\n\
         Respond ONLY with:\n\
         {{\"modules\": [\
         {{\"slug\": \"kebab-slug\", \"title\": \"Human Title\", \"description\": \"one sentence\"}}\
         ]}}"
    );
    let req = LlmRequest {
        system,
        user,
        model: model.to_string(),
        max_tokens,
        timeout_secs,
    };
    let resp = adapter.call(api_key, &req)?;
    parse_outline_response(&resp.text)
}

#[allow(clippy::too_many_arguments)] // LLM-enrichment context bundle; refactor tracked with wiki rework
fn call_assignment_llm(
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    outline: &[OutlineModule],
    batch_evidence: &str,
    batch_num: usize,
    total_batches: usize,
) -> Result<Vec<ModuleProposal>> {
    let module_list = outline
        .iter()
        .map(|m| format!("  - {}: {}", m.slug, m.description))
        .collect::<Vec<_>>()
        .join("\n");

    let system = crate::llm::prompts::COMMUNITY_ASSIGN_SYSTEM_PROMPT.to_string();

    let user = format!(
        "ESTABLISHED MODULES:\n{module_list}\n\n\
         COMMUNITIES TO ASSIGN (batch {batch_num} of {total_batches}):\n\
         {batch_evidence}\n\n\
         Assign every community_id above to one module slug from the established list.\n\
         Respond ONLY with:\n\
         {{\"modules\": [\
         {{\"slug\": \"<established-slug>\", \"title\": \"<title>\", \
         \"description\": \"<desc>\", \"community_ids\": [\"Community:N\", ...]}}\
         ]}}"
    );
    let req = LlmRequest {
        system,
        user,
        model: model.to_string(),
        max_tokens,
        timeout_secs,
    };
    let resp = adapter.call(api_key, &req)?;
    parse_grouping_response(&resp.text)
}

// ── Two-phase batching ───────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)] // LLM-enrichment context bundle; refactor tracked with wiki rework
fn two_phase_grouping(
    graph: &WikiGraph,
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    detailed_evidence: &str,
    estimated_modules: usize,
) -> Result<Vec<ModuleProposal>> {
    // Phase 1: get a global module outline from compressed summary
    let outline_evidence = build_outline_evidence(graph);
    tracing::info!("LLM grouping phase 1: requesting module outline");
    let outline = call_outline_llm(
        adapter,
        api_key,
        model,
        max_tokens,
        timeout_secs,
        &outline_evidence,
        estimated_modules,
    )?;
    tracing::info!(modules = outline.len(), "LLM grouping phase 1 complete");

    // Phase 2: assign communities to the established outline in batches
    let chunks = super::split_text_chunks(detailed_evidence, MAX_EVIDENCE_CHARS);
    let total = chunks.len();
    let mut all: Vec<ModuleProposal> = Vec::new();

    for (i, chunk) in chunks.iter().enumerate() {
        tracing::info!(batch = i + 1, total, "LLM grouping phase 2 batch");
        let mut proposals = call_assignment_llm(
            adapter,
            api_key,
            model,
            max_tokens,
            timeout_secs,
            &outline,
            chunk,
            i + 1,
            total,
        )?;
        all.append(&mut proposals);
    }

    // Fill in title/description from outline for any slug that only has community_ids
    let outline_map: HashMap<&str, &OutlineModule> =
        outline.iter().map(|m| (m.slug.as_str(), m)).collect();
    for p in &mut all {
        if let Some(om) = outline_map.get(p.slug.as_str()) {
            if p.title.is_empty() {
                p.title.clone_from(&om.title);
            }
            if p.description.is_empty() {
                p.description.clone_from(&om.description);
            }
        }
    }

    Ok(merge_proposals(all))
}

// ── Prompt builders ──────────────────────────────────────────────────────────

fn build_grouping_system(estimated_modules: usize) -> String {
    crate::llm::prompts::GROUPING_SYSTEM_PROMPT_TEMPLATE
        .replace("{estimated_modules}", &estimated_modules.to_string())
}

fn build_grouping_user(evidence: &str) -> String {
    format!(
        "Group these code communities into product modules:\n\n\
         {evidence}\n\n\
         Respond ONLY with:\n\
         {{\"modules\": [\
         {{\"slug\": \"kebab-slug\", \"title\": \"Human Title\", \
         \"description\": \"one sentence\", \"community_ids\": [\"Community:N\"]}}\
         ]}}"
    )
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn estimate_module_count(graph: &WikiGraph) -> usize {
    // Count distinct non-trivial feature hints as a baseline
    let mut hints: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for comm in &graph.community_nodes {
        if let Some(hint) = comm
            .props
            .as_ref()
            .and_then(|p| p.get("feature"))
            .and_then(|v| v.as_str())
        {
            // Exclude generic technical folder names
            if !matches!(
                hint,
                "repo"
                    | "service"
                    | "services"
                    | "dto"
                    | "entity"
                    | "entities"
                    | "util"
                    | "utils"
                    | "common"
                    | "shared"
                    | "config"
                    | "mapper"
                    | "mappers"
            ) {
                hints.insert(hint);
            }
        }
    }
    // Aim for the number of meaningful hints, clamped to a sensible range
    hints.len().clamp(8, 40)
}

pub fn merge_proposals(mut proposals: Vec<ModuleProposal>) -> Vec<ModuleProposal> {
    let mut map: HashMap<String, ModuleProposal> = HashMap::new();
    for p in proposals.drain(..) {
        map.entry(p.slug.clone())
            .and_modify(|existing| {
                existing
                    .community_ids
                    .extend(p.community_ids.iter().cloned());
            })
            .or_insert(p);
    }
    let mut result: Vec<ModuleProposal> = map.into_values().collect();
    result.sort_by(|a, b| a.slug.cmp(&b.slug));
    result
}

fn proposals_to_tree(
    proposals: Vec<ModuleProposal>,
    graph: &WikiGraph,
    graph_version: &str,
    community_version: &str,
) -> Result<WikiModuleTree> {
    let mut assigned: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut modules: Vec<WikiModuleNode> = Vec::new();

    for (i, p) in proposals.into_iter().enumerate() {
        let valid_ids: Vec<String> = p
            .community_ids
            .into_iter()
            .filter(|id| graph.nodes_by_id.contains_key(id.as_str()))
            .collect();
        if valid_ids.is_empty() {
            continue;
        }
        for id in &valid_ids {
            assigned.insert(id.clone());
        }
        modules.push(WikiModuleNode {
            id: format!("llm-{}", i),
            slug: p.slug,
            title: p.title,
            description: Some(p.description),
            community_ids: valid_ids,
            file_paths: Vec::new(),
            children: Vec::new(),
        });
    }

    // Any unassigned communities fall into a "shared" catch-all
    let unassigned: Vec<String> = graph
        .community_nodes
        .iter()
        .map(|n| n.id.as_str().to_string())
        .filter(|id| !assigned.contains(id))
        .collect();
    if !unassigned.is_empty() {
        modules.push(WikiModuleNode {
            id: "shared".to_string(),
            slug: "shared".to_string(),
            title: "Shared".to_string(),
            description: Some("Communities not assigned to a specific module.".to_string()),
            community_ids: unassigned,
            file_paths: Vec::new(),
            children: Vec::new(),
        });
    }

    Ok(WikiModuleTree {
        schema_version: 1,
        generated_at: cih_core::now_rfc3339(),
        source: ModuleTreeSource::Llm,
        repo_commit: None,
        graph_version: graph_version.to_string(),
        community_version: community_version.to_string(),
        modules,
    })
}

// ── Parsers ──────────────────────────────────────────────────────────────────

pub fn parse_grouping_response(text: &str) -> Result<Vec<ModuleProposal>> {
    if let Ok(resp) = serde_json::from_str::<GroupingResponse>(text.trim()) {
        return Ok(resp.modules);
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) {
        if start < end {
            if let Ok(resp) = serde_json::from_str::<GroupingResponse>(&text[start..=end]) {
                return Ok(resp.modules);
            }
        }
    }
    bail!(
        "failed to parse LLM grouping response: {:?}",
        &text[..text.len().min(300)]
    )
}

pub fn parse_outline_response(text: &str) -> Result<Vec<OutlineModule>> {
    if let Ok(resp) = serde_json::from_str::<OutlineResponse>(text.trim()) {
        return Ok(resp.modules);
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) {
        if start < end {
            if let Ok(resp) = serde_json::from_str::<OutlineResponse>(&text[start..=end]) {
                return Ok(resp.modules);
            }
        }
    }
    bail!(
        "failed to parse LLM outline response: {:?}",
        &text[..text.len().min(300)]
    )
}

// ── Tests ────────────────────────────────────────────────────────────────────
