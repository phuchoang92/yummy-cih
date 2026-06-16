use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use cih_core::{GraphArtifacts, Node, RepoMap, VersionId};
use cih_wiki::{generate_wiki, CommunityLlmSummary, WikiGraph, WikiInput, WikiLlmInfo};
use rayon::prelude::*;

use crate::llm::evidence::{build_evidence_pack, EvidenceCorpus};
use crate::llm::{make_adapter, resolve_api_key, LlmAdapter, LlmRequest};

pub fn run_wiki(
    repo: &Path,
    out: Option<PathBuf>,
    run_llm: bool,
    llm_provider: &str,
    llm_provider_config: Option<PathBuf>,
    llm_api_key_env: Option<String>,
    evidence_paths: Vec<PathBuf>,
    llm_base_url: &str,
    llm_model: &str,
    llm_max_tokens: u32,
    llm_timeout_secs: u64,
    llm_retries: u32,
    llm_concurrency: usize,
    llm_debug_evidence: bool,
    llm_dry_run: bool,
    wiki_language: &str,
    json: bool,
) -> Result<()> {
    if wiki_language != "en" && wiki_language != "vi" {
        bail!("--wiki-language must be 'en' or 'vi'");
    }

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

    let mut llm_info: Option<WikiLlmInfo> = None;
    let llm_summaries: Option<HashMap<String, CommunityLlmSummary>> = if run_llm {
        let wiki_graph = WikiGraph::build(&nodes, &edges, &community_nodes, &community_edges);
        let adapter = make_adapter(llm_provider, llm_base_url, llm_provider_config.as_deref())?;
        let api_key = resolve_api_key(llm_api_key_env.as_deref())?;
        let evidence_corpus = EvidenceCorpus::load(&evidence_paths)?;

        if llm_debug_evidence {
            println!(
                "[llm-debug] {} communities to enrich, provider={}, model={}, base_url={}, evidence_files={}",
                community_nodes.len(),
                llm_provider,
                llm_model,
                llm_base_url,
                evidence_corpus.file_count
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
                        repo,
                        &evidence_corpus,
                        adapter.as_ref(),
                        api_key.as_deref(),
                        llm_model,
                        llm_max_tokens,
                        llm_timeout_secs,
                        llm_retries,
                        wiki_language,
                        llm_debug_evidence,
                        llm_dry_run,
                    );
                    (comm.id.as_str().to_string(), r)
                })
                .collect()
        });

        let mut map = HashMap::new();
        let mut failed_community_ids = Vec::new();
        for (id, result) in results {
            match result {
                Ok(summary) => {
                    map.insert(id, summary);
                }
                Err(err) => {
                    tracing::warn!(community = %id, error = %err, "LLM enrichment failed");
                    failed_community_ids.push(id);
                }
            }
        }
        failed_community_ids.sort();
        llm_info = Some(WikiLlmInfo {
            provider: llm_provider.to_string(),
            model: llm_model.to_string(),
            language: wiki_language.to_string(),
            evidence_file_count: evidence_corpus.file_count,
            enriched_community_count: map.len(),
            failed_community_count: failed_community_ids.len(),
            failed_community_ids,
        });
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

    let llm_info_for_output = llm_info.clone();

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
        llm_info,
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
                "llm": llm_info_for_output,
            }))?
        );
    } else {
        println!("Wiki generated → {}", outcome.out_dir.display());
        println!(
            "  {} pages · {} communities · {} routes",
            outcome.page_count, outcome.community_count, outcome.route_count
        );
        println!("  Manifest: {}", outcome.manifest_path.display());
        if let Some(info) = llm_info_for_output {
            println!(
                "  LLM enrichment: active (provider={}, model={}, enriched={}, failed={})",
                info.provider,
                info.model,
                info.enriched_community_count,
                info.failed_community_count
            );
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
    repo: &Path,
    evidence_corpus: &EvidenceCorpus,
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    retries: u32,
    language: &str,
    debug_evidence: bool,
    dry_run: bool,
) -> Result<CommunityLlmSummary> {
    let evidence_pack = build_evidence_pack(Some(repo), graph, community, evidence_corpus);
    let evidence = evidence_pack.render();
    let system = build_system_prompt(language);
    let user = build_enrich_prompt(&community.name, &evidence);

    if debug_evidence {
        println!(
            "--- [evidence] community: {} ({}) ---",
            evidence_pack.community_name, evidence_pack.community_id
        );
        println!("{}", evidence);
        return Ok(CommunityLlmSummary {
            po: format!("[debug-evidence] {}", community.name),
            ba: String::new(),
            dev: String::new(),
        });
    }

    if dry_run {
        println!("--- [dry-run] community: {} ---", community.name);
        println!("System:\n{}\n", system);
        println!("User:\n{}", user);
        return Ok(CommunityLlmSummary {
            po: format!("[dry-run] {}", community.name),
            ba: String::new(),
            dev: String::new(),
        });
    }

    let request = LlmRequest {
        system,
        user,
        model: model.to_string(),
        max_tokens,
        timeout_secs,
    };

    let mut last_err = None;
    for attempt in 0..=(retries as usize) {
        match adapter
            .call(api_key, &request)
            .and_then(|response| parse_llm_summary(&response.text))
        {
            Ok(summary) => return Ok(summary),
            Err(err) => {
                if attempt < retries as usize {
                    tracing::debug!(
                        attempt = attempt + 1,
                        error = %err,
                        "LLM call failed, retrying"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(
                        500 * (attempt + 1) as u64,
                    ));
                    last_err = Some(err);
                } else {
                    return Err(err);
                }
            }
        }
    }
    Err(last_err.unwrap())
}

fn build_system_prompt(language: &str) -> String {
    let mut prompt = String::from(
        "You are a code documentation assistant. Write only from the provided evidence.\n\
Do not invent behavior, routes, tables, or class names not in the evidence.\n\
Cite evidence IDs (R1, T1, S1, B1, ...) when they support a claim.",
    );
    if language == "vi" {
        prompt.push_str("\nWrite all documentation in Vietnamese.");
    }
    prompt
}

fn build_enrich_prompt(name: &str, evidence: &str) -> String {
    let evidence = if evidence.trim().is_empty() {
        "none"
    } else {
        evidence
    };
    format!(
        r#"You are writing documentation summaries from a code analysis graph.
Module: "{name}"

Evidence:
{evidence}

Write exactly three JSON fields:
{{
  "po": "<2-3 sentences, plain business language, cite evidence IDs>",
  "ba": "<2-3 sentences, workflows and contracts, cite evidence IDs>",
  "dev": "<2-3 sentences, technical structure, cite evidence IDs>"
}}
Only output the JSON object. Do not add commentary."#
    )
}

fn parse_llm_summary(text: &str) -> Result<CommunityLlmSummary> {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        if let Some(summary) = summary_from_value(&val) {
            return Ok(summary);
        }
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) {
        if start < end {
            let json_str = &text[start..=end];
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(summary) = summary_from_value(&val) {
                    return Ok(summary);
                }
            }
        }
    }
    bail!(
        "failed to extract JSON from LLM response: {:?}",
        &text[..text.len().min(200)]
    )
}

fn summary_from_value(val: &serde_json::Value) -> Option<CommunityLlmSummary> {
    let po = val["po"].as_str().unwrap_or("").to_string();
    let ba = val["ba"].as_str().unwrap_or("").to_string();
    let dev = val["dev"].as_str().unwrap_or("").to_string();
    if po.is_empty() && ba.is_empty() && dev.is_empty() {
        None
    } else {
        Some(CommunityLlmSummary { po, ba, dev })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrich_prompt_contains_community_name_and_routes() {
        let prompt = build_enrich_prompt(
            "order-service",
            "[R1] GET /api/orders\n[D1] Called by: payment-service; calls into: notification-service",
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
    fn parse_llm_summary_errors_on_empty_json_fields() {
        let result = parse_llm_summary(r#"{"po": "", "ba": "", "dev": ""}"#);
        assert!(result.is_err(), "empty response should return Err");
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
