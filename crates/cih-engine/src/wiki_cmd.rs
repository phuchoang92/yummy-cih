use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use cih_core::{GraphArtifacts, Node, RepoMap, VersionId};
use cih_wiki::{CommunityLlmSummary, WikiGraph, WikiInput, generate_wiki};
use rayon::prelude::*;

pub fn run_wiki(
    repo: &Path,
    out: Option<PathBuf>,
    run_llm: bool,
    llm_base_url: &str,
    llm_model: &str,
    llm_timeout_secs: u64,
    llm_retries: u32,
    llm_concurrency: usize,
    llm_debug_evidence: bool,
    llm_dry_run: bool,
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
        let content = std::fs::read_to_string(&repo_map_path)
            .with_context(|| format!("failed to read {}", repo_map_path.display()))?;
        Some(
            serde_json::from_str(&content)
                .with_context(|| format!("failed to parse {}", repo_map_path.display()))?,
        )
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

    // Resolve API key: CIH_LLM_API_KEY > OPENAI_API_KEY > ANTHROPIC_API_KEY
    let api_key = std::env::var("CIH_LLM_API_KEY")
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        .ok();

    if run_llm && !llm_dry_run && api_key.is_none() {
        bail!(
            "--llm requires an API key in CIH_LLM_API_KEY, OPENAI_API_KEY, or ANTHROPIC_API_KEY"
        );
    }

    let llm_summaries: Option<HashMap<String, CommunityLlmSummary>> = if run_llm {
        let wiki_graph = WikiGraph::build(&nodes, &edges, &community_nodes, &community_edges);
        let key = api_key.as_deref().unwrap_or("");

        if llm_debug_evidence {
            println!(
                "[llm-debug] {} communities to enrich, model={}, base_url={}",
                community_nodes.len(),
                llm_model,
                llm_base_url
            );
        }

        let concurrency = llm_concurrency.max(1).min(32);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(concurrency)
            .build()
            .context("failed to build rayon thread pool")?;

        let results: Vec<(String, Result<CommunityLlmSummary>)> = pool.install(|| {
            community_nodes
                .par_iter()
                .map(|comm| {
                    let r = enrich_one_community(
                        comm,
                        &wiki_graph,
                        llm_base_url,
                        key,
                        llm_model,
                        llm_timeout_secs,
                        llm_retries,
                        llm_dry_run,
                    );
                    (comm.id.as_str().to_string(), r)
                })
                .collect()
        });

        let mut map = HashMap::new();
        for (id, result) in results {
            match result {
                Ok(summary) => {
                    map.insert(id, summary);
                }
                Err(err) => {
                    tracing::warn!(community = %id, error = %err, "LLM enrichment failed");
                }
            }
        }
        Some(map)
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
        Some(llm_model.to_string())
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
            println!("  LLM enrichment: active (model={})", llm_model);
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

fn enrich_one_community(
    community: &Node,
    graph: &WikiGraph,
    base_url: &str,
    api_key: &str,
    model: &str,
    timeout_secs: u64,
    retries: u32,
    dry_run: bool,
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

    if dry_run {
        println!("--- [dry-run] community: {} ---", community.name);
        println!("{}", prompt);
        return Ok(CommunityLlmSummary {
            po: format!("[dry-run] {}", community.name),
            ba: String::new(),
            dev: String::new(),
        });
    }

    let is_anthropic = base_url.contains("anthropic.com");

    let mut last_err = None;
    for attempt in 0..=(retries as usize) {
        let result = if is_anthropic {
            call_anthropic(base_url, api_key, model, &prompt, timeout_secs)
        } else {
            call_openai_compat(base_url, api_key, model, &prompt, timeout_secs)
        };

        match result {
            Ok(text) => {
                return parse_llm_summary(&text);
            }
            Err(err) => {
                if attempt < retries as usize {
                    tracing::debug!(
                        attempt = attempt + 1,
                        error = %err,
                        "LLM call failed, retrying"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(500 * (attempt + 1) as u64));
                    last_err = Some(err);
                } else {
                    return Err(err);
                }
            }
        }
    }
    Err(last_err.unwrap())
}

fn call_openai_compat(
    base_url: &str,
    api_key: &str,
    model: &str,
    prompt: &str,
    timeout_secs: u64,
) -> Result<String> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 400,
        "messages": [{"role": "user", "content": prompt}]
    });

    let response = ureq::post(&url)
        .set("Authorization", &format!("Bearer {}", api_key))
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .send_json(body)
        .context("OpenAI-compatible API request failed")?;

    let resp: serde_json::Value =
        response.into_json().context("failed to parse API response")?;

    resp["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .with_context(|| format!("unexpected response shape: {:?}", resp))
}

fn call_anthropic(
    base_url: &str,
    api_key: &str,
    model: &str,
    prompt: &str,
    timeout_secs: u64,
) -> Result<String> {
    let url = format!("{}/messages", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 400,
        "messages": [{"role": "user", "content": prompt}]
    });

    let response = ureq::post(&url)
        .set("x-api-key", api_key)
        .set("anthropic-version", "2023-06-01")
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .send_json(body)
        .context("Anthropic API request failed")?;

    let resp: serde_json::Value =
        response.into_json().context("failed to parse Anthropic API response")?;

    resp["content"][0]["text"]
        .as_str()
        .map(|s| s.to_string())
        .with_context(|| format!("unexpected Anthropic response shape: {:?}", resp))
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
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        let po = val["po"].as_str().unwrap_or("").to_string();
        let ba = val["ba"].as_str().unwrap_or("").to_string();
        let dev = val["dev"].as_str().unwrap_or("").to_string();
        if !po.is_empty() || !ba.is_empty() || !dev.is_empty() {
            return Ok(CommunityLlmSummary { po, ba, dev });
        }
    }
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
    fn parse_llm_summary_errors_on_malformed_response() {
        let result = parse_llm_summary("Not JSON at all");
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
        let text =
            "Here is the summary:\n```json\n{\"po\": \"A\", \"ba\": \"B\", \"dev\": \"C\"}\n```";
        let result = parse_llm_summary(text).unwrap();
        assert_eq!(result.po, "A");
    }
}
