use std::collections::HashMap;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::llm::{LlmAdapter, LlmRequest};
use cih_wiki::{ModuleTreeSource, WikiGraph, WikiModuleNode, WikiModuleTree};

const MAX_EVIDENCE_CHARS: usize = 20_000;

#[derive(Serialize, Deserialize, Debug)]
struct GroupingResponse {
    modules: Vec<ModuleProposal>,
}

#[derive(Serialize, Deserialize, Debug)]
struct ModuleProposal {
    slug: String,
    title: String,
    description: String,
    community_ids: Vec<String>,
}

/// Call the LLM to propose a module tree for `graph`.
/// Batches if total evidence exceeds MAX_EVIDENCE_CHARS.
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
    let evidence = build_grouping_evidence(graph);

    let proposals = if evidence.len() <= MAX_EVIDENCE_CHARS {
        call_grouping_llm(adapter, api_key, model, max_tokens, timeout_secs, &evidence)?
    } else {
        // Batch by chunks of MAX_EVIDENCE_CHARS
        batch_grouping_llm(
            graph,
            adapter,
            api_key,
            model,
            max_tokens,
            timeout_secs,
            &evidence,
        )?
    };

    proposals_to_tree(proposals, graph, graph_version, community_version)
}

fn build_grouping_evidence(graph: &WikiGraph) -> String {
    let mut lines: Vec<String> = Vec::new();
    for comm in &graph.community_nodes {
        let comm_id = comm.id.as_str();
        let route_count = graph
            .community_routes
            .get(comm_id)
            .map(|r| r.len())
            .unwrap_or(0);
        let class_count = graph
            .community_class_counts
            .get(comm_id)
            .copied()
            .unwrap_or(0);
        lines.push(format!(
            "- {} (id={}, routes={}, classes={})",
            comm.name, comm_id, route_count, class_count
        ));
    }
    lines.join("\n")
}

fn call_grouping_llm(
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    evidence: &str,
) -> Result<Vec<ModuleProposal>> {
    let req = LlmRequest {
        system: build_grouping_system(),
        user: build_grouping_user(evidence),
        model: model.to_string(),
        max_tokens,
        timeout_secs,
    };
    let resp = adapter.call(api_key, &req)?;
    parse_grouping_response(&resp.text)
}

fn batch_grouping_llm(
    _graph: &WikiGraph,
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    evidence: &str,
) -> Result<Vec<ModuleProposal>> {
    // Split evidence into chunks
    let chunks = split_into_chunks(evidence, MAX_EVIDENCE_CHARS);
    let mut all: Vec<ModuleProposal> = Vec::new();
    for (i, chunk) in chunks.iter().enumerate() {
        tracing::info!(batch = i + 1, total = chunks.len(), "LLM grouping batch");
        let mut proposals =
            call_grouping_llm(adapter, api_key, model, max_tokens, timeout_secs, chunk)?;
        all.append(&mut proposals);
    }
    // Merge: combine communities assigned to the same slug
    Ok(merge_proposals(all))
}

fn split_into_chunks(text: &str, max_chars: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in lines {
        if current.len() + line.len() + 1 > max_chars && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(text.to_string());
    }
    chunks
}

fn merge_proposals(mut proposals: Vec<ModuleProposal>) -> Vec<ModuleProposal> {
    let mut map: HashMap<String, ModuleProposal> = HashMap::new();
    for p in proposals.drain(..) {
        map.entry(p.slug.clone())
            .and_modify(|existing| {
                existing
                    .community_ids
                    .extend(p.community_ids.iter().cloned())
            })
            .or_insert(p);
    }
    let mut result: Vec<ModuleProposal> = map.into_values().collect();
    result.sort_by(|a, b| a.slug.cmp(&b.slug));
    result
}

fn build_grouping_system() -> String {
    "You are a software architect grouping code communities into product modules.\n\
     Group related communities into cohesive modules. Each module should represent a \
     distinct product capability or bounded context. Respond with a JSON object only."
        .to_string()
}

fn build_grouping_user(evidence: &str) -> String {
    format!(
        r#"Given these code communities, propose a module grouping:

{evidence}

Respond ONLY with a JSON object:
{{
  "modules": [
    {{
      "slug": "kebab-case-slug",
      "title": "Human Readable Title",
      "description": "One sentence description",
      "community_ids": ["Community:0", "Community:1"]
    }}
  ]
}}"#
    )
}

fn parse_grouping_response(text: &str) -> Result<Vec<ModuleProposal>> {
    // Try direct parse
    if let Ok(resp) = serde_json::from_str::<GroupingResponse>(text.trim()) {
        return Ok(resp.modules);
    }
    // Extract JSON block
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

    // Assign any unassigned communities to a "shared" catch-all node
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_grouping_response_extracts_modules() {
        let text = r#"{"modules": [{"slug": "order", "title": "Order", "description": "Handles orders", "community_ids": ["Community:0"]}]}"#;
        let result = parse_grouping_response(text).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].slug, "order");
    }

    #[test]
    fn parse_grouping_response_handles_json_in_prose() {
        let text = r#"Here is the grouping: {"modules": [{"slug": "pay", "title": "Pay", "description": "d", "community_ids": ["Community:1"]}]}"#;
        let result = parse_grouping_response(text).unwrap();
        assert_eq!(result[0].slug, "pay");
    }

    #[test]
    fn parse_grouping_response_errors_on_malformed() {
        assert!(parse_grouping_response("not json").is_err());
    }

    #[test]
    fn split_into_chunks_respects_max() {
        let lines = (0..100)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = split_into_chunks(&lines, 200);
        assert!(chunks.len() > 1, "should produce multiple chunks");
        for chunk in &chunks {
            assert!(chunk.len() <= 250, "chunk should not greatly exceed limit");
        }
    }

    #[test]
    fn merge_proposals_combines_duplicate_slugs() {
        let proposals = vec![
            ModuleProposal {
                slug: "order".to_string(),
                title: "Order".to_string(),
                description: "d".to_string(),
                community_ids: vec!["Community:0".to_string()],
            },
            ModuleProposal {
                slug: "order".to_string(),
                title: "Order".to_string(),
                description: "d".to_string(),
                community_ids: vec!["Community:1".to_string()],
            },
        ];
        let merged = merge_proposals(proposals);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].community_ids.len(), 2);
    }
}
