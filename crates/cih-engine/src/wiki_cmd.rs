use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{bail, Context, Result};
use cih_core::{GraphArtifacts, Node, RepoMap, VersionId};
use cih_wiki::{
    generate_wiki, CommunityLlmSummary, WikiGenerationInfo, WikiGraph, WikiInput, WikiLlmInfo,
    WikiMeta, WikiModuleCacheEntry, WikiModuleTree,
};
use rayon::prelude::*;

use crate::llm::evidence::{build_evidence_pack, EvidenceCorpus};
use crate::llm::{backoff_ms, make_adapter, redact_key, resolve_api_key, LlmAdapter, LlmRequest};

/// FNV-1a 64-bit hash (no external dependency).
fn fnv64(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}

/// Load existing wiki_meta.json if present (for incremental mode).
fn load_wiki_meta(out_dir: &Path) -> Option<WikiMeta> {
    let path = out_dir.join("wiki_meta.json");
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

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
    wiki_mode: &str,
    grouping: &str,
    html: bool,
    incremental: bool,
    save_evidence: bool,
    filter_community: Vec<String>,
    max_communities: Option<usize>,
    json: bool,
) -> Result<()> {
    if wiki_language != "en" && wiki_language != "vi" {
        bail!("--wiki-language must be 'en' or 'vi'");
    }
    let effective_run_llm = run_llm || wiki_mode == "llm-summary" || wiki_mode == "llm-full";
    if !["graph", "llm-summary", "llm-full"].contains(&wiki_mode) {
        bail!("--wiki-mode must be one of: graph, llm-summary, llm-full");
    }
    if !["graph", "llm"].contains(&grouping) {
        bail!("--grouping must be one of: graph, llm");
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
    let all_community_nodes = community_artifacts.read_nodes().with_context(|| {
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

    // Apply --filter-community and --max-communities before any LLM or wiki work.
    let community_nodes: Vec<Node> = {
        let mut filtered = all_community_nodes;
        if !filter_community.is_empty() {
            let filters_lower: Vec<String> = filter_community.iter().map(|f| f.to_lowercase()).collect();
            filtered.retain(|n| {
                let name_lower = n.name.to_lowercase();
                filters_lower.iter().any(|f| name_lower.contains(f.as_str()))
            });
        }
        if let Some(max) = max_communities {
            filtered.truncate(max);
        }
        filtered
    };

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

    let out_dir = out.unwrap_or_else(|| repo.join(".cih").join("wiki"));
    let repo_name = repo
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let mut llm_info: Option<WikiLlmInfo> = None;
    let mut summaries_for_cache: Vec<(String, String, CommunityLlmSummary)> = Vec::new();
    let llm_summaries: Option<HashMap<String, CommunityLlmSummary>> = if effective_run_llm {
        let wiki_graph = WikiGraph::build(&nodes, &edges, &community_nodes, &community_edges);
        let adapter = make_adapter(llm_provider, llm_base_url, llm_provider_config.as_deref())?;
        let api_key = resolve_api_key(llm_api_key_env.as_deref())?;
        let evidence_corpus = EvidenceCorpus::load(&evidence_paths)?;

        // Incremental: load previous wiki_meta.json to find unchanged communities.
        let prev_meta: Option<WikiMeta> = if incremental {
            load_wiki_meta(&out_dir)
        } else {
            None
        };

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

        const CIRCUIT_BREAKER_THRESHOLD: u32 = 5;
        let consecutive_failures = AtomicU32::new(0);
        let done_count = AtomicU32::new(0);
        let total = community_nodes.len();

        eprintln!("[cih-wiki] enriching {} communities (concurrency={}, model={})", total, concurrency, llm_model);

        // Results: (comm_id, evidence_hash, Result<summary>)
        let results: Vec<(String, String, Result<CommunityLlmSummary>)> = pool.install(|| {
            community_nodes
                .par_iter()
                .map(|comm| {
                    let comm_id = comm.id.as_str().to_string();
                    let pack = build_evidence_pack(Some(repo), &wiki_graph, comm, &evidence_corpus);
                    let ev_hash = fnv64(&pack.render());

                    // Incremental: check evidence hash against previous run.
                    if let Some(meta) = &prev_meta {
                        if let Some(cached) = meta.module_cache.get(&comm_id) {
                            if cached.evidence_hash == ev_hash {
                                if let (Some(po), Some(ba), Some(dev)) =
                                    (&cached.llm_po, &cached.llm_ba, &cached.llm_dev)
                                {
                                    let done = done_count.fetch_add(1, Ordering::Relaxed) + 1;
                                    eprintln!("[{}/{}] {} — cached (skipped)", done, total, comm.name);
                                    return (
                                        comm_id,
                                        ev_hash,
                                        Ok(CommunityLlmSummary {
                                            po: po.clone(),
                                            ba: ba.clone(),
                                            dev: dev.clone(),
                                        }),
                                    );
                                }
                            }
                        }
                    }

                    if consecutive_failures.load(Ordering::Relaxed) >= CIRCUIT_BREAKER_THRESHOLD {
                        let done = done_count.fetch_add(1, Ordering::Relaxed) + 1;
                        eprintln!("[{}/{}] {} — SKIPPED (circuit open)", done, total, comm.name);
                        return (
                            comm_id,
                            ev_hash,
                            Err(anyhow::anyhow!("CIRCUIT_OPEN: skipped after consecutive failures")),
                        );
                    }

                    eprintln!("[cih-wiki] calling LLM for: {}", comm.name);
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
                    let done = done_count.fetch_add(1, Ordering::Relaxed) + 1;
                    match &r {
                        Ok(_) => eprintln!("[{}/{}] {} — ok", done, total, comm.name),
                        Err(e) => eprintln!("[{}/{}] {} — FAILED: {}", done, total, comm.name, e),
                    }
                    if r.is_err() {
                        consecutive_failures.fetch_add(1, Ordering::Relaxed);
                    } else {
                        consecutive_failures.store(0, Ordering::Relaxed);
                    }
                    (comm_id, ev_hash, r)
                })
                .collect()
        });

        let mut map: HashMap<String, CommunityLlmSummary> = HashMap::new();
        // evidence_hash_map: community_id -> hash (for cache write)
        let mut ev_hash_map: HashMap<String, String> = HashMap::new();
        let mut failed_community_ids = Vec::new();
        let mut circuit_open = false;
        for (id, ev_hash, result) in results {
            ev_hash_map.insert(id.clone(), ev_hash);
            match result {
                Ok(summary) => {
                    map.insert(id, summary);
                }
                Err(err) => {
                    let err_str = err.to_string();
                    if err_str.contains("CIRCUIT_OPEN") {
                        circuit_open = true;
                    }
                    let redacted = redact_key(&err_str, api_key.as_deref());
                    tracing::warn!(community = %id, error = %redacted, "LLM enrichment failed");
                    failed_community_ids.push(id);
                }
            }
        }
        failed_community_ids.sort();
        if circuit_open {
            tracing::warn!("LLM circuit breaker opened after {} consecutive failures; remaining communities skipped", CIRCUIT_BREAKER_THRESHOLD);
        }
        // Stash summaries + hashes for post-generation wiki_meta update.
        summaries_for_cache = map
            .iter()
            .filter_map(|(id, s)| ev_hash_map.get(id).map(|h| (id.clone(), h.clone(), s.clone())))
            .collect();
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

    // LLM grouping: propose a module tree via LLM before page generation
    let llm_module_tree: Option<WikiModuleTree> = if grouping == "llm" {
        let wiki_graph = WikiGraph::build(&nodes, &edges, &community_nodes, &community_edges);
        let adapter = make_adapter(llm_provider, llm_base_url, llm_provider_config.as_deref())?;
        let api_key = resolve_api_key(llm_api_key_env.as_deref())?;
        match crate::llm::grouping::propose_module_tree(
            &wiki_graph,
            adapter.as_ref(),
            api_key.as_deref(),
            llm_model,
            llm_max_tokens,
            llm_timeout_secs,
            &graph_artifacts.version.0,
            &community_artifacts.version.0,
        ) {
            Ok(tree) => {
                tracing::info!(modules = tree.modules.len(), "LLM grouping proposed {} modules", tree.modules.len());
                Some(tree)
            }
            Err(err) => {
                tracing::warn!(error = %err, "LLM grouping failed; falling back to graph grouping");
                None
            }
        }
    } else {
        None
    };

    // Collect evidence packs for --save-evidence
    let save_evidence_map: Option<HashMap<String, String>> = if save_evidence {
        let wiki_graph = WikiGraph::build(&nodes, &edges, &community_nodes, &community_edges);
        let evidence_corpus = crate::llm::evidence::EvidenceCorpus::load(&evidence_paths)?;
        let map: HashMap<String, String> = community_nodes
            .iter()
            .map(|comm| {
                let pack = crate::llm::evidence::build_evidence_pack(
                    Some(repo),
                    &wiki_graph,
                    comm,
                    &evidence_corpus,
                );
                (comm.id.as_str().to_string(), pack.render())
            })
            .collect();
        Some(map)
    } else {
        None
    };

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
        llm_full: None,
        llm_info,
        module_tree: llm_module_tree,
        generation: WikiGenerationInfo {
            mode: wiki_mode.to_string(),
            grouping: grouping.to_string(),
            review_required: false,
            html_viewer: html,
            incremental,
        },
        first_module_tree: None,
        save_evidence: save_evidence_map,
    };

    let outcome = generate_wiki(input, &out_dir)?;

    // Update wiki_meta.json with evidence hashes and cached LLM summaries.
    if !summaries_for_cache.is_empty() {
        if let Some(mut meta) = load_wiki_meta(&out_dir) {
            for (id, hash, summary) in &summaries_for_cache {
                let entry = meta.module_cache.entry(id.clone()).or_insert_with(|| WikiModuleCacheEntry {
                    content_hash: String::new(),
                    evidence_hash: String::new(),
                    page_paths: Vec::new(),
                    llm_po: None,
                    llm_ba: None,
                    llm_dev: None,
                });
                entry.evidence_hash = hash.clone();
                entry.llm_po = Some(summary.po.clone());
                entry.llm_ba = Some(summary.ba.clone());
                entry.llm_dev = Some(summary.dev.clone());
            }
            if let Ok(json) = serde_json::to_string_pretty(&meta) {
                let _ = std::fs::write(out_dir.join("wiki_meta.json"), json);
            }
        }
    }

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

    // Jitter seed derived from community name (deterministic, no thread-rng).
    let jitter_seed: u64 = community
        .id
        .as_str()
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));

    let mut last_err = None;
    for attempt in 0..=(retries as usize) {
        match adapter
            .call(api_key, &request)
            .and_then(|response| parse_llm_summary(&response.text))
        {
            Ok(summary) => return Ok(summary),
            Err(err) => {
                if attempt < retries as usize {
                    let delay = backoff_ms(attempt, jitter_seed.wrapping_add(attempt as u64));
                    tracing::debug!(
                        attempt = attempt + 1,
                        delay_ms = delay,
                        error = %err,
                        "LLM call failed, retrying"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(delay));
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
