use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{bail, Context, Result};
use cih_core::{GraphArtifacts, Node, RepoMap, VersionId};
use cih_wiki::features::{group_communities_by_feature, FeatureGroup};
use cih_wiki::graph::{route_http_method, route_path};
use cih_wiki::{
    generate_wiki, CommunityLlmFull, CommunityLlmSummary, ControllerLlmSummary, FeatureLlmSummary,
    FeatureMetaEntry, WikiGenerationInfo, WikiGraph, WikiInput, WikiLlmInfo, WikiMeta,
    WikiModuleCacheEntry, WikiModuleTree,
};
use rayon::prelude::*;

use crate::llm::evidence::{build_evidence_pack, EvidenceCorpus};
use crate::llm::{backoff_ms, make_adapter, redact_key, resolve_api_key, LlmAdapter, LlmRequest};
use crate::ui::PhaseProgress;

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

pub struct WikiConfig {
    pub repo: PathBuf,
    pub out: Option<PathBuf>,
    pub run_llm: bool,
    pub llm_provider: String,
    pub llm_provider_config: Option<PathBuf>,
    pub llm_api_key_env: Option<String>,
    pub evidence_paths: Vec<PathBuf>,
    pub llm_base_url: String,
    pub llm_model: String,
    pub llm_max_tokens: u32,
    pub llm_timeout_secs: u64,
    pub llm_retries: u32,
    pub llm_concurrency: usize,
    pub llm_debug_evidence: bool,
    pub llm_dry_run: bool,
    pub wiki_language: String,
    pub wiki_mode: String,
    pub grouping: String,
    pub html: bool,
    pub incremental: bool,
    pub save_evidence: bool,
    pub filter_community: Vec<String>,
    pub max_communities: Option<usize>,
    pub filter_feature: Vec<String>,
    pub json: bool,
}

impl Default for WikiConfig {
    fn default() -> Self {
        Self {
            repo: PathBuf::new(),
            out: None,
            run_llm: false,
            llm_provider: "openai-compatible".into(),
            llm_provider_config: None,
            llm_api_key_env: None,
            evidence_paths: vec![],
            llm_base_url: "https://api.openai.com/v1".into(),
            llm_model: String::new(),
            llm_max_tokens: 1024,
            llm_timeout_secs: 30,
            llm_retries: 2,
            llm_concurrency: 4,
            llm_debug_evidence: false,
            llm_dry_run: false,
            wiki_language: "en".into(),
            wiki_mode: "graph".into(),
            grouping: "graph".into(),
            html: false,
            incremental: false,
            save_evidence: false,
            filter_community: vec![],
            max_communities: None,
            filter_feature: vec![],
            json: false,
        }
    }
}

pub fn run_wiki(cfg: WikiConfig) -> Result<()> {
    let WikiConfig {
        repo,
        out,
        run_llm,
        llm_provider,
        llm_provider_config,
        llm_api_key_env,
        evidence_paths,
        llm_base_url,
        llm_model,
        llm_max_tokens,
        llm_timeout_secs,
        llm_retries,
        llm_concurrency,
        llm_debug_evidence,
        llm_dry_run,
        wiki_language,
        wiki_mode,
        grouping,
        html,
        incremental,
        save_evidence,
        filter_community,
        max_communities,
        filter_feature,
        json,
    } = cfg;
    let repo = repo.as_path();
    let llm_provider = llm_provider.as_str();
    let llm_base_url = llm_base_url.as_str();
    let llm_model = llm_model.as_str();
    let wiki_language = wiki_language.as_str();
    let wiki_mode = wiki_mode.as_str();
    let grouping = grouping.as_str();
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
    let llm_no_call = llm_dry_run || llm_debug_evidence;

    let span = tracing::info_span!("wiki", repo = %repo.display());
    let _enter = span.enter();

    tracing::info!(
        repo = %repo.display(),
        mode = wiki_mode,
        grouping = grouping,
        llm = effective_run_llm,
        "starting wiki"
    );

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

    tracing::info!(
        graph_version = %graph_artifacts.version.0,
        nodes = nodes.len(),
        edges = edges.len(),
        "graph artifacts loaded"
    );

    let bodies = cih_wiki::source_bodies(&nodes, repo);

    let (all_community_nodes, community_edges, community_version) = match latest_community_artifacts(
        repo,
    ) {
        Ok(a) => {
            let nodes = a.read_nodes().with_context(|| {
                format!(
                    "failed to read community nodes from {}",
                    a.nodes_path.display()
                )
            })?;
            let edges = a.read_edges().with_context(|| {
                format!(
                    "failed to read community edges from {}",
                    a.edges_path.display()
                )
            })?;
            let ver = a.version.0.clone();
            tracing::info!(
                community_version = %ver,
                communities = nodes.len(),
                "community artifacts loaded"
            );
            (nodes, edges, ver)
        }
        Err(_) => {
            tracing::info!("no community artifacts found — generating wiki without feature grouping; run `discover` first for richer docs");
            eprintln!(
                "info: no community artifacts found — generating wiki without feature grouping. \
                     Run `discover` first for richer docs."
            );
            (Vec::new(), Vec::new(), String::new())
        }
    };

    // Apply --filter-community and --max-communities before any LLM or wiki work.
    let community_nodes: Vec<Node> = {
        let before = all_community_nodes.len();
        let mut filtered = all_community_nodes;
        if !filter_community.is_empty() {
            let filters_lower: Vec<String> =
                filter_community.iter().map(|f| f.to_lowercase()).collect();
            filtered.retain(|n| {
                let name_lower = n.name.to_lowercase();
                filters_lower
                    .iter()
                    .any(|f| name_lower.contains(f.as_str()))
            });
        }
        if let Some(max) = max_communities {
            filtered.truncate(max);
        }
        if filtered.len() != before {
            tracing::info!(
                before = before,
                after = filtered.len(),
                filter_community = ?filter_community,
                max_communities = ?max_communities,
                "community filter applied"
            );
        }
        filtered
    };

    // Build WikiGraph once; all LLM paths and save_evidence share it.
    let wiki_graph = WikiGraph::build(&nodes, &edges, &community_nodes, &community_edges);

    // Create adapter + API key once for all LLM paths.
    let (adapter, api_key): (Option<Box<dyn LlmAdapter>>, Option<String>) =
        if effective_run_llm || grouping == "llm" {
            let a = make_adapter(llm_provider, llm_base_url, llm_provider_config.as_deref())?;
            let k = if llm_no_call {
                None
            } else {
                resolve_api_key(llm_api_key_env.as_deref())?
            };
            (Some(a), k)
        } else {
            (None, None)
        };

    // Load evidence corpus once; used by community enrichment, llm-full, and save_evidence.
    let evidence_corpus = EvidenceCorpus::load(&evidence_paths)?;

    // Build rayon thread pool once; shared by community and llm-full enrichment.
    let (pool, concurrency) = if effective_run_llm {
        let c = llm_concurrency.max(1).min(32);
        let p = rayon::ThreadPoolBuilder::new()
            .num_threads(c)
            .build()
            .context("failed to build rayon thread pool")?;
        (Some(p), c)
    } else {
        (None, 0usize)
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
    let repo_name = std::fs::canonicalize(&repo)
        .unwrap_or_else(|_| repo.to_path_buf())
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let repo_name_display = repo_name.clone();

    let mut llm_info: Option<WikiLlmInfo> = None;
    let mut summaries_for_cache: Vec<(String, String, CommunityLlmSummary)> = Vec::new();
    let (llm_summaries, controller_summaries): (
        Option<HashMap<String, CommunityLlmSummary>>,
        Option<HashMap<String, ControllerLlmSummary>>,
    ) = if effective_run_llm {
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

        const CIRCUIT_BREAKER_THRESHOLD: u32 = 5;
        let consecutive_failures = AtomicU32::new(0);
        let total = community_nodes.len();

        tracing::info!(
            communities = total,
            concurrency = concurrency,
            model = llm_model,
            provider = llm_provider,
            "starting LLM community enrichment"
        );

        let mut ui = PhaseProgress::new();
        if json { ui.hide(); }
        ui.start_phase("Enriching communities", Some(total as u64));

        // community enrichment (par) + controller enrichment (sequential) run concurrently
        let (community_raw, ctrl): (
            Vec<(String, String, Result<CommunityLlmSummary>)>,
            Option<HashMap<String, ControllerLlmSummary>>,
        ) = pool.as_ref().unwrap().install(|| {
            rayon::join(
                || {
                    let result = community_nodes
                        .par_iter()
                        .map(|comm| {
                            let comm_id = comm.id.as_str().to_string();
                            let pack = build_evidence_pack(
                                Some(repo),
                                &wiki_graph,
                                comm,
                                &evidence_corpus,
                            );
                            let ev_hash = fnv64(&pack.render());

                            // Incremental: check evidence hash against previous run.
                            if let Some(summary) =
                                cached_summary(&comm_id, &ev_hash, prev_meta.as_ref())
                            {
                                ui.tick_skipped(format!("{} (cached)", &comm.name));
                                return (comm_id, ev_hash, Ok(summary));
                            }

                            if is_circuit_open(
                                consecutive_failures.load(Ordering::Relaxed),
                                CIRCUIT_BREAKER_THRESHOLD,
                            ) {
                                ui.tick_failed(format!("{} (circuit open)", &comm.name));
                                return (
                                    comm_id,
                                    ev_hash,
                                    Err(anyhow::anyhow!(
                                        "CIRCUIT_OPEN: skipped after consecutive failures"
                                    )),
                                );
                            }

                            ui.tick(comm.name.as_str());
                            let r = enrich_one_community(
                                comm,
                                &wiki_graph,
                                repo,
                                &evidence_corpus,
                                adapter.as_ref().unwrap().as_ref(),
                                api_key.as_deref(),
                                llm_model,
                                llm_max_tokens,
                                llm_timeout_secs,
                                llm_retries,
                                wiki_language,
                                llm_debug_evidence,
                                llm_dry_run,
                            );
                            if r.is_err() {
                                consecutive_failures.fetch_add(1, Ordering::Relaxed);
                                ui.inc_failed();
                            } else {
                                consecutive_failures.store(0, Ordering::Relaxed);
                                ui.inc_ok();
                            }
                            (comm_id, ev_hash, r)
                        })
                        .collect::<Vec<_>>();
                    ui.finish_phase();
                    result
                },
                || {
                    tracing::info!(
                        controllers = wiki_graph.routes_by_controller.len(),
                        "starting LLM controller enrichment"
                    );
                    let r = enrich_controllers(
                        &wiki_graph,
                        adapter.as_ref().unwrap().as_ref(),
                        api_key.as_deref(),
                        llm_model,
                        llm_max_tokens,
                        llm_timeout_secs,
                        wiki_language,
                        llm_dry_run || llm_debug_evidence,
                    );
                    tracing::info!(enriched = r.len(), "LLM controller enrichment complete");
                    Some(r)
                },
            )
        });

        let mut map: HashMap<String, CommunityLlmSummary> = HashMap::new();
        // evidence_hash_map: community_id -> hash (for cache write)
        let mut ev_hash_map: HashMap<String, String> = HashMap::new();
        let mut failed_community_ids = Vec::new();
        let mut circuit_open = false;
        for (id, ev_hash, result) in community_raw {
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
        tracing::info!(
            enriched = map.len(),
            failed = failed_community_ids.len(),
            circuit_open = circuit_open,
            "LLM community enrichment complete"
        );
        if circuit_open {
            tracing::warn!("LLM circuit breaker opened after {} consecutive failures; remaining communities skipped", CIRCUIT_BREAKER_THRESHOLD);
        }
        // Stash summaries + hashes for post-generation wiki_meta update.
        summaries_for_cache = map
            .iter()
            .filter_map(|(id, s)| {
                ev_hash_map
                    .get(id)
                    .map(|h| (id.clone(), h.clone(), s.clone()))
            })
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
        (Some(map), ctrl)
    } else {
        (None, None)
    };

    // llm-full: additional richer per-community content for dev + BA pages.
    let llm_full_map: Option<HashMap<String, CommunityLlmFull>> =
        if wiki_mode == "llm-full" && llm_no_call {
            tracing::info!("skipping llm-full enrichment because dry-run/debug mode is enabled");
            None
        } else if wiki_mode == "llm-full" {
            let total_full = community_nodes.len();
            tracing::info!(communities = total_full, "starting LLM full enrichment");

            let mut ui_full = PhaseProgress::new();
            if json { ui_full.hide(); }
            ui_full.start_phase("Deep enrichment (PO/BA)", Some(total_full as u64));

            let results: Vec<(String, Result<CommunityLlmFull>)> =
                pool.as_ref().unwrap().install(|| {
                    community_nodes
                        .par_iter()
                        .map(|comm| {
                            ui_full.tick(comm.name.as_str());
                            let r = enrich_one_community_full(
                                comm,
                                &wiki_graph,
                                repo,
                                &evidence_corpus,
                                adapter.as_ref().unwrap().as_ref(),
                                api_key.as_deref(),
                                llm_model,
                                llm_max_tokens,
                                llm_timeout_secs,
                                llm_retries,
                                wiki_language,
                            );
                            if r.is_ok() { ui_full.inc_ok(); } else { ui_full.inc_failed(); }
                            (comm.id.as_str().to_string(), r)
                        })
                        .collect()
                });
            ui_full.finish_phase();

            let mut map = HashMap::new();
            for (id, result) in results {
                match result {
                    Ok(full) => {
                        map.insert(id, full);
                    }
                    Err(err) => {
                        tracing::warn!(community = %id, error = %err, "LLM full enrichment failed")
                    }
                }
            }
            tracing::info!(enriched = map.len(), "LLM full enrichment complete");
            Some(map)
        } else {
            None
        };

    // LLM grouping: propose a module tree via LLM before page generation
    let llm_module_tree: Option<WikiModuleTree> = if grouping == "llm" && llm_no_call {
        tracing::info!("skipping LLM grouping because dry-run/debug mode is enabled");
        None
    } else if grouping == "llm" {
        match crate::llm::grouping::propose_module_tree(
            &wiki_graph,
            adapter.as_ref().unwrap().as_ref(),
            api_key.as_deref(),
            llm_model,
            llm_max_tokens,
            llm_timeout_secs,
            &graph_artifacts.version.0,
            &community_version,
        ) {
            Ok(tree) => {
                tracing::info!(
                    modules = tree.modules.len(),
                    "LLM grouping proposed {} modules",
                    tree.modules.len()
                );
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

    // Feature-level LLM enrichment: one call per wiki feature for PO/BA pages.
    // Runs after community enrichment so community evidence is already computed.
    let mut feature_cache_updates: Vec<(String, String, FeatureLlmSummary)> = Vec::new();
    let feature_llm_map: Option<HashMap<String, FeatureLlmSummary>> = if effective_run_llm {
        let mut feature_groups = group_communities_by_feature(&wiki_graph);
        retain_matching_feature_groups(&mut feature_groups, &filter_feature);
        let prev_meta_for_features: Option<WikiMeta> = if incremental {
            load_wiki_meta(&out_dir)
        } else {
            None
        };

        let active_features: Vec<&FeatureGroup> = feature_groups
            .iter()
            .filter(|g| !g.community_ids.is_empty())
            .collect();

        let mut ui_feat = PhaseProgress::new();
        if json { ui_feat.hide(); }
        ui_feat.start_phase("Enriching features", Some(active_features.len() as u64));

        let mut map: HashMap<String, FeatureLlmSummary> = HashMap::new();

        for group in &active_features {
            let merged_ev =
                build_feature_evidence(&group.community_ids, &wiki_graph, repo, &evidence_corpus);
            let ev_hash = fnv64(&merged_ev);

            // Cache hit?
            if let Some(cached) =
                cached_feature_summary(&group.feature, &ev_hash, prev_meta_for_features.as_ref())
            {
                feature_cache_updates.push((group.feature.clone(), ev_hash, cached.clone()));
                map.insert(group.feature.clone(), cached);
                ui_feat.tick_skipped(format!("{} (cached)", &group.feature));
                continue;
            }

            ui_feat.tick(group.feature.as_str());
            tracing::info!(feature = %group.feature, "calling LLM for feature enrichment");
            match enrich_one_feature(
                &group.feature,
                &merged_ev,
                adapter.as_ref().unwrap().as_ref(),
                api_key.as_deref(),
                llm_model,
                llm_max_tokens,
                llm_timeout_secs,
                llm_debug_evidence,
                llm_dry_run,
            ) {
                Ok(summary) => {
                    feature_cache_updates.push((group.feature.clone(), ev_hash, summary.clone()));
                    map.insert(group.feature.clone(), summary);
                    ui_feat.inc_ok();
                }
                Err(err) => {
                    tracing::warn!(feature = %group.feature, error = %err, "feature LLM enrichment failed");
                    ui_feat.inc_failed();
                }
            }
        }

        ui_feat.finish_phase();
        tracing::info!(features = map.len(), "feature LLM enrichment complete");
        if map.is_empty() {
            None
        } else {
            Some(map)
        }
    } else {
        None
    };

    // Collect evidence packs for --save-evidence
    let save_evidence_map: Option<HashMap<String, String>> = if save_evidence {
        let map: HashMap<String, String> = community_nodes
            .iter()
            .map(|comm| {
                let pack = build_evidence_pack(Some(repo), &wiki_graph, comm, &evidence_corpus);
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
        community_version,
        unresolved_report,
        repo_map,
        llm_summaries,
        llm_full: llm_full_map,
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
        controller_summaries,
        feature_llm_summaries: feature_llm_map,
        filter_feature,
        bodies,
    };

    tracing::info!(out_dir = %out_dir.display(), "generating wiki pages");
    let mut ui_gen = crate::ui::PhaseProgress::new();
    if json { ui_gen.hide(); }
    ui_gen.spin("Generating pages");
    let outcome = generate_wiki(input, &out_dir)?;
    ui_gen.finish_with(format!("{} pages", outcome.page_count));

    tracing::info!(
        pages = outcome.page_count,
        communities = outcome.community_count,
        routes = outcome.route_count,
        llm_enriched = outcome.llm_enriched,
        out_dir = %outcome.out_dir.display(),
        "wiki generation complete"
    );

    persist_wiki_meta_caches(&out_dir, &summaries_for_cache, &feature_cache_updates)?;

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
        crate::ui::print_header("Wiki", &repo_name_display, None);
        crate::ui::print_row("Pages", &outcome.page_count.to_string());
        crate::ui::print_row(
            "Communities",
            &format!("{}  routes {}", outcome.community_count, outcome.route_count),
        );
        if let Some(ref info) = llm_info_for_output {
            crate::ui::print_row(
                "LLM",
                &format!(
                    "{}  ·  {}  enriched {}  failed {}",
                    info.provider,
                    info.model,
                    info.enriched_community_count,
                    info.failed_community_count
                ),
            );
        }
        crate::ui::print_row("Output", &outcome.out_dir.display().to_string());
        crate::ui::print_row("Manifest", &outcome.manifest_path.display().to_string());
        eprintln!();
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
Cite evidence IDs (R1, P1, T1, S1, B1, ...) inline when they support a claim.",
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
  "po": "<2-3 sentences, plain business language, cite evidence IDs like [R1],[P1]>",
  "ba": "<2-3 sentences, workflows and contracts, cite evidence IDs like [R1],[P1]>",
  "dev": "<2-3 sentences, technical structure, cite evidence IDs like [R1],[P1]>"
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

fn build_full_prompt(name: &str, evidence: &str) -> String {
    let evidence = if evidence.trim().is_empty() {
        "none"
    } else {
        evidence
    };
    format!(
        r#"You are writing detailed documentation from a code analysis graph.
Module: "{name}"

Evidence:
{evidence}

Write exactly ten JSON fields (2–4 sentences each, cite evidence IDs):
{{
  "po_summary": "<business purpose and value>",
  "po_capabilities": "<key business capabilities exposed>",
  "po_workflows": "<end-to-end user-facing workflows>",
  "po_open_questions": "<gaps or assumptions needing clarification>",
  "ba_process_overview": "<high-level process flow>",
  "ba_contracts": "<API and event contracts with other modules>",
  "ba_business_rules": "<validations, rules, and invariants>",
  "dev_responsibility": "<what this module owns in the system>",
  "dev_key_classes": "<central classes and their roles>",
  "dev_entry_points": "<primary entry points: routes, listeners, scheduled tasks>"
}}
Only output the JSON object. Do not add commentary."#
    )
}

fn parse_llm_full(text: &str) -> Result<CommunityLlmFull> {
    let try_extract = |val: &serde_json::Value| -> Option<CommunityLlmFull> {
        let f = |key: &str| val[key].as_str().unwrap_or("").to_string();
        let full = CommunityLlmFull {
            po_summary: f("po_summary"),
            po_capabilities: f("po_capabilities"),
            po_workflows: f("po_workflows"),
            po_open_questions: f("po_open_questions"),
            ba_process_overview: f("ba_process_overview"),
            ba_contracts: f("ba_contracts"),
            ba_business_rules: f("ba_business_rules"),
            dev_responsibility: f("dev_responsibility"),
            dev_key_classes: f("dev_key_classes"),
            dev_entry_points: f("dev_entry_points"),
        };
        let all_empty = full.po_summary.is_empty()
            && full.po_capabilities.is_empty()
            && full.ba_process_overview.is_empty()
            && full.dev_responsibility.is_empty();
        if all_empty {
            None
        } else {
            Some(full)
        }
    };

    if let Ok(val) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        if let Some(full) = try_extract(&val) {
            return Ok(full);
        }
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) {
        if start < end {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text[start..=end]) {
                if let Some(full) = try_extract(&val) {
                    return Ok(full);
                }
            }
        }
    }
    bail!(
        "failed to extract llm-full JSON from response: {:?}",
        &text[..text.len().min(200)]
    )
}

fn enrich_one_community_full(
    community: &cih_core::Node,
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
) -> Result<CommunityLlmFull> {
    let evidence_pack = build_evidence_pack(Some(repo), graph, community, evidence_corpus);
    let evidence = evidence_pack.render();
    let mut system = String::from(
        "You are a code documentation assistant. Write only from the provided evidence.\n\
         Do not invent behavior, routes, tables, or class names not in the evidence.\n\
         Cite evidence IDs (R1, T1, S1, B1, ...) when they support a claim.",
    );
    if language == "vi" {
        system.push_str("\nWrite all documentation in Vietnamese.");
    }
    let user = build_full_prompt(&community.name, &evidence);
    let request = LlmRequest {
        system,
        user,
        model: model.to_string(),
        max_tokens,
        timeout_secs,
    };
    let jitter_seed: u64 = community
        .id
        .as_str()
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    let mut last_err = None;
    for attempt in 0..=(retries as usize) {
        match adapter
            .call(api_key, &request)
            .and_then(|r| parse_llm_full(&r.text))
        {
            Ok(full) => return Ok(full),
            Err(err) => {
                if attempt < retries as usize {
                    let delay = backoff_ms(attempt, jitter_seed.wrapping_add(attempt as u64));
                    tracing::debug!(attempt = attempt + 1, delay_ms = delay, error = %err, "llm-full call failed, retrying");
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

const CONTROLLER_BATCH_SIZE: usize = 10;
const MAX_ROUTES_PER_CONTROLLER: usize = 15;

fn enrich_controllers(
    graph: &WikiGraph,
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    language: &str,
    dry_run: bool,
) -> HashMap<String, ControllerLlmSummary> {
    let mut controllers: Vec<(&String, &Vec<(cih_core::Node, cih_core::Node)>)> =
        graph.routes_by_controller.iter().collect();
    controllers.sort_by_key(|(name, _)| name.as_str());

    if controllers.is_empty() {
        return HashMap::new();
    }

    let n_batches = controllers.chunks(CONTROLLER_BATCH_SIZE).count();
    let mut ui_ctrl = PhaseProgress::new();
    ui_ctrl.start_phase("Enriching controllers", Some(n_batches as u64));

    let mut result = HashMap::new();

    for batch in controllers.chunks(CONTROLLER_BATCH_SIZE) {
        let batch_names: Vec<&str> = batch.iter().map(|(n, _)| n.as_str()).collect();
        ui_ctrl.tick(batch_names.first().copied().unwrap_or("batch"));

        let user_prompt = build_controller_batch_prompt(batch, language);

        if dry_run {
            println!("--- [dry-run] controller batch ---\n{}", user_prompt);
            for (name, _) in batch {
                result.insert(
                    name.to_string(),
                    ControllerLlmSummary {
                        description: format!("[dry-run] {}", name),
                        feature: None,
                    },
                );
            }
            ui_ctrl.inc_ok();
            continue;
        }

        let request = LlmRequest {
            system: build_controller_system_prompt(language),
            user: user_prompt,
            model: model.to_string(),
            max_tokens,
            timeout_secs,
        };

        match adapter.call(api_key, &request) {
            Ok(response) => {
                let batch_result = parse_controller_batch(&response.text);
                result.extend(batch_result);
                ui_ctrl.inc_ok();
            }
            Err(err) => {
                tracing::warn!(error = %err, "controller enrichment batch failed — continuing");
                ui_ctrl.inc_failed();
            }
        }
    }

    ui_ctrl.finish_phase();
    result
}

fn build_controller_system_prompt(language: &str) -> String {
    let mut s = String::from(
        "You are a code documentation assistant. Write concise business descriptions \
         from the provided API route signatures. Do not invent behavior not implied by \
         the route paths and method names.",
    );
    if language == "vi" {
        s.push_str(" Write all descriptions in Vietnamese.");
    }
    s
}

fn build_controller_batch_prompt(
    batch: &[(&String, &Vec<(cih_core::Node, cih_core::Node)>)],
    _language: &str,
) -> String {
    let mut s = String::from(
        "Document these REST API controllers for business stakeholders.\n\
         For each controller provide:\n\
         - \"description\": 1-2 sentences in plain business language\n\
         - \"feature\": business domain slug (e.g. \"payment\", \"auth\", \"order\") \
           inferred from the class name and routes\n\n\
         Respond with a single JSON object only:\n\
         { \"ControllerName\": { \"description\": \"...\", \"feature\": \"slug\" }, ... }\n\n\
         Controllers:\n\n",
    );

    for (ctrl_name, routes) in batch {
        s.push_str(ctrl_name);
        s.push_str(":\n");
        for (handler, route) in routes.iter().take(MAX_ROUTES_PER_CONTROLLER) {
            let method = route_http_method(route);
            let path = route_path(route);
            let handler_name = handler
                .id
                .as_str()
                .split('#')
                .nth(1)
                .and_then(|x| x.split('/').next())
                .unwrap_or("handler");
            s.push_str(&format!("  {} {} — {}\n", method, path, handler_name));
        }
        if routes.len() > MAX_ROUTES_PER_CONTROLLER {
            s.push_str(&format!(
                "  ... and {} more routes\n",
                routes.len() - MAX_ROUTES_PER_CONTROLLER
            ));
        }
        s.push('\n');
    }

    s
}

fn parse_controller_batch(text: &str) -> HashMap<String, ControllerLlmSummary> {
    let mut result = HashMap::new();

    // Strip markdown code fences that some models (e.g. Gemini) add
    let cleaned = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let val: serde_json::Value = if let Ok(v) = serde_json::from_str(cleaned) {
        v
    } else if let (Some(s), Some(e)) = (cleaned.find('{'), cleaned.rfind('}')) {
        if s < e {
            match serde_json::from_str(&cleaned[s..=e]) {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!(error = %err, "failed to parse controller batch JSON");
                    return result;
                }
            }
        } else {
            return result;
        }
    } else {
        return result;
    };

    if let Some(obj) = val.as_object() {
        for (ctrl_name, ctrl_val) in obj {
            let description = ctrl_val["description"].as_str().unwrap_or("").to_string();
            let feature = ctrl_val["feature"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            if !description.is_empty() || feature.is_some() {
                result.insert(
                    ctrl_name.clone(),
                    ControllerLlmSummary {
                        description,
                        feature,
                    },
                );
            }
        }
    }

    result
}

fn is_circuit_open(consecutive: u32, threshold: u32) -> bool {
    consecutive >= threshold
}

fn persist_wiki_meta_caches(
    out_dir: &Path,
    community_updates: &[(String, String, CommunityLlmSummary)],
    feature_updates: &[(String, String, FeatureLlmSummary)],
) -> Result<()> {
    if community_updates.is_empty() && feature_updates.is_empty() {
        return Ok(());
    }

    let meta_path = out_dir.join("wiki_meta.json");
    let text = std::fs::read_to_string(&meta_path)
        .with_context(|| format!("failed to read {}", meta_path.display()))?;
    let mut meta: WikiMeta = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse {}", meta_path.display()))?;

    for (id, hash, summary) in community_updates {
        let entry = meta
            .module_cache
            .entry(id.clone())
            .or_insert_with(|| WikiModuleCacheEntry {
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

    for (feature_name, hash, summary) in feature_updates {
        meta.feature_cache.insert(
            feature_name.clone(),
            FeatureMetaEntry {
                ev_hash: hash.clone(),
                po_overview: summary.po_overview.clone(),
                po_capabilities: summary.po_capabilities.clone(),
                ba_process_overview: summary.ba_process_overview.clone(),
                ba_business_rules: summary.ba_business_rules.clone(),
            },
        );
    }

    let json = serde_json::to_string_pretty(&meta).context("failed to serialize wiki metadata")?;
    std::fs::write(&meta_path, json)
        .with_context(|| format!("failed to write {}", meta_path.display()))?;

    Ok(())
}

fn retain_matching_feature_groups(
    feature_groups: &mut Vec<FeatureGroup>,
    filter_feature: &[String],
) {
    if filter_feature.is_empty() {
        return;
    }
    let filters_lower: Vec<String> = filter_feature.iter().map(|f| f.to_lowercase()).collect();
    feature_groups.retain(|group| {
        let name = group.feature.to_lowercase();
        filters_lower.iter().any(|filter| name.contains(filter))
    });
}

/// Build merged evidence text for a feature by concatenating evidence packs from all
/// communities in the feature. Deduplicates route and table items; caps at 6 000 chars.
fn build_feature_evidence(
    community_ids: &[String],
    graph: &WikiGraph,
    repo: &Path,
    corpus: &EvidenceCorpus,
) -> String {
    const MAX_FEATURE_EVIDENCE: usize = 6_000;
    let mut seen_texts = std::collections::BTreeSet::new();
    let mut merged = String::new();

    for (community_idx, comm_id) in community_ids.iter().enumerate() {
        let Some(comm_node) = graph.nodes_by_id.get(comm_id) else {
            continue;
        };
        let pack = build_evidence_pack(Some(repo), graph, comm_node, corpus);
        if pack.items.is_empty() {
            continue;
        }
        let community_evidence_id = format!("C{}", community_idx + 1);
        let section_header = format!("# {community_evidence_id} Community: {}\n", comm_node.name);
        let mut section = String::new();
        for item in &pack.items {
            if seen_texts.contains(&item.text) {
                continue;
            }
            seen_texts.insert(item.text.clone());
            section.push_str(&format!(
                "[{}-{}] {}\n",
                community_evidence_id,
                item.id,
                item.text.trim()
            ));
        }
        if section.is_empty() {
            continue;
        }
        if merged.len() + section_header.len() + section.len() > MAX_FEATURE_EVIDENCE {
            break;
        }
        merged.push_str(&section_header);
        merged.push_str(&section);
        merged.push('\n');
    }
    merged
}

/// Call the LLM once for a whole feature to get a cohesive PO/BA overview.
fn enrich_one_feature(
    feature: &str,
    evidence: &str,
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    debug_evidence: bool,
    dry_run: bool,
) -> Result<FeatureLlmSummary> {
    let evidence_str = if evidence.trim().is_empty() {
        "none"
    } else {
        evidence
    };
    let system = build_feature_system_prompt();
    let user = build_feature_user_prompt(feature, evidence_str);

    if debug_evidence {
        println!("--- [feature evidence] feature: {} ---", feature);
        println!("{}", evidence_str);
        return Ok(FeatureLlmSummary {
            po_overview: format!("[debug-evidence] {}", feature),
            po_capabilities: String::new(),
            ba_process_overview: String::new(),
            ba_business_rules: String::new(),
        });
    }

    if dry_run {
        println!("--- [dry-run] feature: {} ---", feature);
        println!("System:\n{}\n", system);
        println!("User:\n{}", user);
        return Ok(FeatureLlmSummary {
            po_overview: format!("[dry-run] {}", feature),
            po_capabilities: String::new(),
            ba_process_overview: String::new(),
            ba_business_rules: String::new(),
        });
    }

    let req = LlmRequest {
        system,
        user,
        model: model.to_string(),
        max_tokens,
        timeout_secs,
    };
    let resp = adapter.call(api_key, &req)?;
    parse_feature_summary(&resp.text)
}

fn build_feature_system_prompt() -> String {
    "You are a software architect writing business documentation from code evidence.\n\
     Write only from the provided evidence. Cite evidence IDs exactly as shown, like [C1-R1],[C1-P1],[C2-B1].\n\
     Do not invent behavior not in the evidence."
        .to_string()
}

fn build_feature_user_prompt(feature: &str, evidence: &str) -> String {
    format!(
        r#"You are writing feature-level documentation for the "{feature}" module.

Evidence (grouped by community):
{evidence}

Respond ONLY with a JSON object:
{{
  "po_overview": "<3-5 sentences of plain-language business overview>",
  "po_capabilities": "<bullet list of business capabilities, one per line starting with ->",
  "ba_process_overview": "<3-5 sentences describing business processes and flows>",
  "ba_business_rules": "<key business rules or invariants, one per line starting with ->>"
}}"#
    )
}

fn parse_feature_summary(text: &str) -> Result<FeatureLlmSummary> {
    let json_str = if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) {
        if start <= end {
            &text[start..=end]
        } else {
            text.trim()
        }
    } else {
        text.trim()
    };
    let val: serde_json::Value = serde_json::from_str(json_str).map_err(|e| {
        anyhow::anyhow!(
            "failed to parse feature LLM response: {e}: {:?}",
            &text[..text.len().min(200)]
        )
    })?;
    let summary = FeatureLlmSummary {
        po_overview: val["po_overview"].as_str().unwrap_or("").to_string(),
        po_capabilities: val["po_capabilities"].as_str().unwrap_or("").to_string(),
        ba_process_overview: val["ba_process_overview"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        ba_business_rules: val["ba_business_rules"].as_str().unwrap_or("").to_string(),
    };
    if summary.po_overview.is_empty()
        && summary.po_capabilities.is_empty()
        && summary.ba_process_overview.is_empty()
        && summary.ba_business_rules.is_empty()
    {
        bail!("feature LLM response did not contain any expected fields");
    }
    Ok(summary)
}

fn cached_feature_summary(
    feature: &str,
    ev_hash: &str,
    meta: Option<&WikiMeta>,
) -> Option<FeatureLlmSummary> {
    let entry = meta?.feature_cache.get(feature)?;
    if entry.ev_hash != ev_hash {
        return None;
    }
    Some(FeatureLlmSummary {
        po_overview: entry.po_overview.clone(),
        po_capabilities: entry.po_capabilities.clone(),
        ba_process_overview: entry.ba_process_overview.clone(),
        ba_business_rules: entry.ba_business_rules.clone(),
    })
}

fn cached_summary(
    comm_id: &str,
    ev_hash: &str,
    meta: Option<&WikiMeta>,
) -> Option<CommunityLlmSummary> {
    let cached = meta?.module_cache.get(comm_id)?;
    if cached.evidence_hash != ev_hash {
        return None;
    }
    let po = cached.llm_po.clone()?;
    let ba = cached.llm_ba.clone()?;
    let dev = cached.llm_dev.clone()?;
    Some(CommunityLlmSummary { po, ba, dev })
}

#[cfg(test)]
mod tests;

