use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use cih_core::{GraphArtifacts, Node, RepoMap, VersionId};
use cih_wiki::{CommunityLlmSummary, WikiGraph, WikiInput, generate_wiki};

pub fn run_wiki(
    repo: &Path,
    out: Option<PathBuf>,
    llm_enrich: bool,
    llm_model: String,
    json: bool,
) -> Result<()> {
    let graph_artifacts = crate::versioning::latest_graph_artifacts(repo)?;
    let nodes = graph_artifacts.read_nodes().with_context(|| {
        format!(
            "failed to read nodes from {}",
            graph_artifacts.nodes_path.display()
        )
    })?;
    let edges = graph_artifacts.read_edges().with_context(|| {
        format!(
            "failed to read edges from {}",
            graph_artifacts.edges_path.display()
        )
    })?;

    let community_artifacts = latest_community_artifacts(repo)?;
    let community_nodes = community_artifacts.read_nodes().with_context(|| {
        format!(
            "failed to read community nodes from {}",
            community_artifacts.nodes_path.display()
        )
    })?;
    let community_edges = community_artifacts.read_edges().with_context(|| {
        format!(
            "failed to read community edges from {}",
            community_artifacts.edges_path.display()
        )
    })?;

    let repo_map_path = repo.join(".cih").join("repo-map.json");
    let repo_map: Option<RepoMap> = if repo_map_path.is_file() {
        let content = std::fs::read_to_string(&repo_map_path).with_context(|| {
            format!("failed to read {}", repo_map_path.display())
        })?;
        Some(serde_json::from_str(&content).with_context(|| {
            format!("failed to parse {}", repo_map_path.display())
        })?)
    } else {
        None
    };

    let unresolved_path = graph_artifacts
        .nodes_path
        .parent()
        .map(|p| p.join("unresolved-refs.md"));
    let unresolved_report: Option<String> = unresolved_path.and_then(|p| {
        if p.is_file() {
            std::fs::read_to_string(&p).ok()
        } else {
            None
        }
    });

    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();

    if llm_enrich && api_key.is_none() {
        bail!("--llm-enrich requires ANTHROPIC_API_KEY to be set in the environment");
    }

    let llm_summaries: Option<HashMap<String, CommunityLlmSummary>> = if llm_enrich {
        let key = api_key.unwrap();
        let wiki_graph = WikiGraph::build(&nodes, &edges, &community_nodes, &community_edges);
        Some(enrich_communities(&wiki_graph.community_nodes, &wiki_graph, &key, &llm_model))
    } else {
        None
    };

    let out_dir = out.unwrap_or_else(|| repo.join(".cih").join("wiki"));
    let repo_name = repo
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let llm_model_opt = if llm_summaries.is_some() {
        Some(llm_model)
    } else {
        None
    };

    let input = WikiInput {
        nodes: &nodes,
        edges: &edges,
        community_nodes: &community_nodes,
        community_edges: &community_edges,
        repo_name,
        graph_version: graph_artifacts.version.0.clone(),
        community_version: community_artifacts.version.0.clone(),
        unresolved_report,
        repo_map,
        llm_summaries,
        llm_model: llm_model_opt,
    };

    let outcome = generate_wiki(input, &out_dir)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "out_dir": outcome.out_dir.display().to_string(),
                "manifest_path": outcome.manifest_path.display().to_string(),
                "page_count": outcome.page_count,
                "community_count": outcome.community_count,
                "route_count": outcome.route_count,
                "llm_enriched": outcome.llm_enriched,
            }))?
        );
    } else {
        println!("Wiki generated → {}", outcome.out_dir.display());
        println!(
            "  {} pages · {} communities · {} routes",
            outcome.page_count, outcome.community_count, outcome.route_count
        );
        println!("  Manifest: {}", outcome.manifest_path.display());
        if outcome.llm_enriched {
            println!("  LLM enrichment: active");
        }
    }

    Ok(())
}

fn latest_community_artifacts(repo: &Path) -> Result<GraphArtifacts> {
    let parent = repo.join(".cih").join("artifacts-community");
    let mut candidates = Vec::new();
    let entries = std::fs::read_dir(&parent).with_context(|| {
        format!(
            "no community artifacts at {} - run `discover` first",
            parent.display()
        )
    })?;
    for entry in entries {
        let entry = entry?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let nodes_path = dir.join("nodes.jsonl");
        let edges_path = dir.join("edges.jsonl");
        if !nodes_path.is_file() || !edges_path.is_file() {
            continue;
        }
        let version = entry.file_name().to_string_lossy().into_owned();
        let modified = std::fs::metadata(&nodes_path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        candidates.push((
            modified,
            GraphArtifacts {
                nodes_path,
                edges_path,
                version: VersionId(version),
            },
        ));
    }
    candidates.sort_by(|(a, _), (b, _)| b.cmp(a));
    candidates
        .into_iter()
        .next()
        .map(|(_, a)| a)
        .with_context(|| format!("no complete community artifacts under {}", parent.display()))
}

pub fn enrich_communities(
    community_nodes: &[Node],
    graph: &WikiGraph,
    api_key: &str,
    model: &str,
) -> HashMap<String, CommunityLlmSummary> {
    let mut result = HashMap::new();
    for comm in community_nodes {
        match enrich_one_community(comm, graph, api_key, model) {
            Ok(summary) => {
                result.insert(comm.id.as_str().to_string(), summary);
            }
            Err(err) => {
                tracing::warn!(
                    community = %comm.name,
                    error = %err,
                    "LLM enrichment failed for community"
                );
            }
        }
    }
    result
}

fn enrich_one_community(
    community: &Node,
    graph: &WikiGraph,
    api_key: &str,
    model: &str,
) -> Result<CommunityLlmSummary> {
    use cih_wiki::graph::{route_http_method, route_path};

    let comm_id = community.id.as_str();

    let routes: Vec<String> = graph
        .community_routes
        .get(comm_id)
        .map(|rs| {
            rs.iter()
                .take(5)
                .map(|(_, r)| format!("{} {}", route_http_method(r), route_path(r)))
                .collect()
        })
        .unwrap_or_default();

    let stereo_str = graph
        .community_stereotypes
        .get(comm_id)
        .map(|s| {
            s.iter()
                .map(|(k, v)| format!("{} {}", v, k))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| "—".to_string());

    let caller_names: Vec<String> = graph
        .callers_of(comm_id)
        .iter()
        .map(|(id, _)| graph.community_name(id).to_string())
        .collect();
    let callee_names: Vec<String> = graph
        .callees_of(comm_id)
        .iter()
        .map(|(id, _)| graph.community_name(id).to_string())
        .collect();

    let route_str = if routes.is_empty() {
        "none".to_string()
    } else {
        routes.join(", ")
    };
    let caller_str = if caller_names.is_empty() {
        "none".to_string()
    } else {
        caller_names.join(", ")
    };
    let callee_str = if callee_names.is_empty() {
        "none".to_string()
    } else {
        callee_names.join(", ")
    };

    let prompt =
        build_enrich_prompt(&community.name, &route_str, &stereo_str, &caller_str, &callee_str);

    let body = serde_json::json!({
        "model": model,
        "max_tokens": 400,
        "messages": [{"role": "user", "content": prompt}]
    });

    let response = ureq::post("https://api.anthropic.com/v1/messages")
        .set("x-api-key", api_key)
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json")
        .send_json(body)
        .context("Anthropic API request failed")?;

    let resp_json: serde_json::Value =
        response.into_json().context("failed to parse Anthropic API response")?;

    let text = resp_json["content"][0]["text"].as_str().unwrap_or("");
    parse_llm_summary(text)
}

fn build_enrich_prompt(
    name: &str,
    routes: &str,
    stereotypes: &str,
    called_by: &str,
    calls_into: &str,
) -> String {
    format!(
        r#"You are writing documentation summaries from a code analysis graph.
Module: "{name}"

Graph facts (do not invent anything beyond these):
- Routes: {routes}
- Class stereotypes: {stereotypes}
- Called by: {called_by}
- Calls into: {calls_into}

Write exactly three JSON fields:
{{
  "po": "<2-3 sentences in plain business language — what this module does for users>",
  "ba": "<2-3 sentences on workflows, contracts, and events — what flows in and out>",
  "dev": "<2-3 sentences on technical structure — stereotypes, call patterns, dependencies>"
}}
Only output the JSON object. Do not add commentary."#
    )
}

fn parse_llm_summary(text: &str) -> Result<CommunityLlmSummary> {
    // Try direct JSON parse
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        let po = val["po"].as_str().unwrap_or("").to_string();
        let ba = val["ba"].as_str().unwrap_or("").to_string();
        let dev = val["dev"].as_str().unwrap_or("").to_string();
        if !po.is_empty() || !ba.is_empty() || !dev.is_empty() {
            return Ok(CommunityLlmSummary { po, ba, dev });
        }
    }
    // Try to find JSON block within text (model may wrap in markdown)
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) {
        if start < end {
            let json_str = &text[start..=end];
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                let po = val["po"].as_str().unwrap_or("").to_string();
                let ba = val["ba"].as_str().unwrap_or("").to_string();
                let dev = val["dev"].as_str().unwrap_or("").to_string();
                return Ok(CommunityLlmSummary { po, ba, dev });
            }
        }
    }
    bail!(
        "failed to extract JSON from LLM response: {:?}",
        &text[..text.len().min(200)]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrich_prompt_contains_community_name_and_routes() {
        let prompt = build_enrich_prompt(
            "order-service",
            "GET /api/orders, POST /api/orders",
            "1 controller, 2 service",
            "payment-service",
            "notification-service",
        );
        assert!(prompt.contains("order-service"));
        assert!(prompt.contains("GET /api/orders"));
        assert!(prompt.contains("payment-service"));
        assert!(prompt.contains("notification-service"));
    }

    #[test]
    fn enrich_communities_skips_community_on_malformed_response() {
        let bad = "Not JSON at all";
        let result = parse_llm_summary(bad);
        assert!(result.is_err(), "malformed response should return Err");
    }

    #[test]
    fn parse_llm_summary_extracts_valid_json() {
        let text = r#"{"po": "Business stuff", "ba": "Flow stuff", "dev": "Tech stuff"}"#;
        let result = parse_llm_summary(text).unwrap();
        assert_eq!(result.po, "Business stuff");
        assert_eq!(result.ba, "Flow stuff");
        assert_eq!(result.dev, "Tech stuff");
    }

    #[test]
    fn parse_llm_summary_handles_json_in_markdown_block() {
        let text = "Here is the summary:\n```json\n{\"po\": \"A\", \"ba\": \"B\", \"dev\": \"C\"}\n```";
        let result = parse_llm_summary(text).unwrap();
        assert_eq!(result.po, "A");
    }
}
