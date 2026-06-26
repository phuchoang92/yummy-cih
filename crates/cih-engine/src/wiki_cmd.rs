use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use cih_core::{Edge, Node, RepoMap};
use cih_wiki::assign_class_slugs;
use cih_wiki::features::{group_communities_by_feature, FeatureGroup};
use cih_wiki::graph::route_path;
use cih_wiki::{
    generate_wiki, ClassCacheEntry, ClassEnrichmentStore, CommunityLlmFull, CommunityLlmSummary,
    ControllerLlmSummary, FeatureLlmSummary, FeatureMetaEntry, FlowCacheEntry, FlowLlmSummary,
    WikiGenerationInfo, WikiGraph, WikiInput, WikiLlmInfo, WikiMeta, WikiModuleCacheEntry,
    WikiModuleTree,
};
use rayon::prelude::*;

use crate::llm::evidence::{build_evidence_pack, EvidenceCorpus};
use crate::llm::{backoff_ms, make_adapter, resolve_api_key, LlmAdapter, LlmRequest};
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

fn load_class_enrichment(cih_dir: &Path) -> ClassEnrichmentStore {
    let path = cih_dir.join("class-enrichment.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return ClassEnrichmentStore::default(),
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_class_enrichment(cih_dir: &Path, store: &ClassEnrichmentStore) -> Result<()> {
    std::fs::create_dir_all(cih_dir)?;
    let tmp = cih_dir.join("class-enrichment.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(store)?)?;
    std::fs::rename(&tmp, cih_dir.join("class-enrichment.json"))?;
    Ok(())
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
    /// Keep only communities that have at least one route whose path starts with
    /// (or contains) one of these patterns. Empty = no filtering.
    pub filter_route: Vec<String>,
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
            grouping: "package".into(),
            html: false,
            incremental: false,
            save_evidence: false,
            filter_community: vec![],
            max_communities: None,
            filter_feature: vec![],
            filter_route: vec![],
            json: false,
        }
    }
}

/// Bundled LLM run parameters — avoids repeating 10 fields across helper functions.
struct LlmRunParams<'a> {
    adapter: &'a dyn LlmAdapter,
    api_key: Option<&'a str>,
    model: &'a str,
    max_tokens: u32,
    timeout_secs: u64,
    retries: u32,
    dry_run: bool,
    language: &'a str,
    debug_evidence: bool,
}

/// Output of `load_wiki_artifacts` — fully-built graph and derived metadata for a wiki run.
struct WikiArtifacts {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    wiki_graph: WikiGraph,
    community_nodes: Vec<Node>,
    community_edges: Vec<Edge>,
    community_version: String,
    graph_version: String,
    repo_map: Option<RepoMap>,
    unresolved_report: Option<String>,
    out_dir: PathBuf,
    repo_name: String,
    bodies: HashMap<String, cih_wiki::BodyEntry>,
    file_dev_map: HashMap<String, String>,
    feature_of: Box<dyn Fn(&str, &str) -> String + Send>,
}

/// Load graph artifacts, build WikiGraph, and collect all static metadata for a wiki run.
/// Returns `None` when `--filter-route` matches nothing (fast early exit, no pages to generate).
fn load_wiki_artifacts(
    repo: &Path,
    out: Option<PathBuf>,
    grouping: &str,
    filter_community: &[String],
    max_communities: Option<usize>,
    filter_route: &[String],
) -> Result<Option<WikiArtifacts>> {
    let graph_artifacts;
    let nodes;
    let edges;
    let wiki_graph;
    let community_nodes: Vec<Node>;
    let community_edges: Vec<Edge>;
    let community_version: String;
    let feature_of: Box<dyn Fn(&str, &str) -> String + Send>;

    if grouping == "package" {
        graph_artifacts = crate::versioning::latest_graph_artifacts(repo)?;
        nodes = graph_artifacts.read_nodes().with_context(|| {
            format!(
                "failed to read nodes from {}",
                graph_artifacts.nodes_path.display()
            )
        })?;
        edges = graph_artifacts.read_edges().with_context(|| {
            format!(
                "failed to read edges from {}",
                graph_artifacts.edges_path.display()
            )
        })?;
        tracing::info!(
            graph_version = %graph_artifacts.version.0,
            nodes = nodes.len(),
            edges = edges.len(),
            "graph artifacts loaded (package mode)"
        );
        let pkg_cfg = cih_grouping::PackageConfig::load_or_default(repo);
        let pkg_strategy: Arc<dyn cih_grouping::FeatureStrategy> =
            Arc::new(cih_grouping::PackageStrategy::new(pkg_cfg));

        let repo_default_feature: Arc<String> = Arc::new({
            let raw = repo
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("shared")
                .to_lowercase();
            let mut s = raw.as_str();
            for suf in &["-api", "-service", "-impl", "-core", "-module", "-web", "-rest"] {
                s = s.strip_suffix(suf).unwrap_or(s);
            }
            for pfx in &[
                "banking-", "payment-", "finance-", "base-", "common-", "core-",
                "shared-", "platform-", "infra-", "app-", "service-",
            ] {
                s = s.strip_prefix(pfx).unwrap_or(s);
            }
            if s.is_empty() || s == "shared" { raw } else { s.to_string() }
        });

        let feature_lookup: Arc<std::collections::HashMap<String, String>> = Arc::new(
            cih_grouping::find_feature_artifact_dir(repo, &graph_artifacts.version.0)
                .and_then(|dir| cih_grouping::read_feature_artifact(&dir).ok())
                .map(|entries| entries.into_iter().map(|e| (e.node_id, e.name)).collect())
                .unwrap_or_default(),
        );
        if !feature_lookup.is_empty() {
            tracing::info!(entries = feature_lookup.len(), "loaded pre-computed feature artifact");
        }

        {
            let s = pkg_strategy.clone();
            let lk = feature_lookup.clone();
            let df = repo_default_feature.clone();
            wiki_graph = WikiGraph::build_package_grouped(&nodes, &edges, &|node_id, f| {
                let feat = lk.get(node_id).cloned().unwrap_or_else(|| s.feature_of(f));
                if feat == "shared" { df.as_ref().clone() } else { feat }
            });
        }
        let all_pkg_nodes: Vec<Node> = wiki_graph.community_nodes.clone();
        community_nodes = filter_communities_by_route(all_pkg_nodes, &wiki_graph, filter_route);
        if !filter_route.is_empty() && community_nodes.is_empty() {
            eprintln!("info: --filter-route matched 0 packages; nothing to generate.");
            return Ok(None);
        }
        community_edges = Vec::new();
        community_version = graph_artifacts.version.0.clone();
        feature_of = Box::new(move |node_id: &str, f: &str| {
            let feat = feature_lookup
                .get(node_id)
                .cloned()
                .unwrap_or_else(|| pkg_strategy.feature_of(f));
            if feat == "shared" { repo_default_feature.as_ref().clone() } else { feat }
        });
    } else {
        let community_artifact = cih_core::GraphArtifacts::latest_in_dir(&repo.join(".cih").join("artifacts-community")).ok();
        let (pre_community_nodes, community_version_raw) = match community_artifact.as_ref() {
            Some(a) => {
                let ns = a.read_nodes().with_context(|| {
                    format!(
                        "failed to read community nodes from {}",
                        a.nodes_path.display()
                    )
                })?;
                let ver = a.version.0.clone();
                tracing::info!(
                    community_version = %ver,
                    communities = ns.len(),
                    "community artifacts loaded"
                );
                (ns, ver)
            }
            None => {
                tracing::info!(
                    "no community artifacts found — generating wiki without feature grouping; \
                     run `discover` first for richer docs"
                );
                eprintln!(
                    "info: no community artifacts found — generating wiki without feature grouping. \
                     Run `discover` first for richer docs."
                );
                (Vec::new(), String::new())
            }
        };

        let community_nodes_pre: Vec<Node> = {
            let before = pre_community_nodes.len();
            let mut filtered = pre_community_nodes;
            if !filter_community.is_empty() {
                let filters_lower: Vec<String> =
                    filter_community.iter().map(|f| f.to_lowercase()).collect();
                filtered.retain(|n| {
                    let name_lower = n.name.to_lowercase();
                    filters_lower.iter().any(|f| name_lower.contains(f.as_str()))
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

        let community_nodes_pre: Vec<Node> = if !filter_route.is_empty() {
            community_nodes_pre
                .into_iter()
                .filter(|n| community_matches_route_prefix(n, filter_route))
                .collect()
        } else {
            community_nodes_pre
        };

        if !filter_route.is_empty()
            && community_artifact.is_some()
            && community_nodes_pre.is_empty()
        {
            eprintln!(
                "info: --filter-route matched 0 communities (pre-filter); nothing to generate."
            );
            return Ok(None);
        }

        graph_artifacts = crate::versioning::latest_graph_artifacts(repo)?;
        nodes = graph_artifacts.read_nodes().with_context(|| {
            format!(
                "failed to read nodes from {}",
                graph_artifacts.nodes_path.display()
            )
        })?;
        edges = graph_artifacts.read_edges().with_context(|| {
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

        let (community_nodes_loaded, community_edges_loaded, cv) = match community_artifact {
            Some(a) => {
                let comm_edges = a.read_edges().with_context(|| {
                    format!(
                        "failed to read community edges from {}",
                        a.edges_path.display()
                    )
                })?;
                (community_nodes_pre, comm_edges, community_version_raw)
            }
            None => (Vec::new(), Vec::new(), String::new()),
        };
        community_version = cv;

        wiki_graph = WikiGraph::build(
            &nodes,
            &edges,
            &community_nodes_loaded,
            &community_edges_loaded,
        );
        community_edges = community_edges_loaded;
        community_nodes =
            filter_communities_by_route(community_nodes_loaded, &wiki_graph, filter_route);
        feature_of = Box::new(|_, _| "shared".to_string());
    }

    let bodies = {
        let member_ids: std::collections::HashSet<&str> = community_nodes
            .iter()
            .flat_map(|c| {
                wiki_graph
                    .members_by_community
                    .get(c.id.as_str())
                    .into_iter()
                    .flatten()
                    .map(|n| n.id.as_str())
            })
            .collect();
        let body_nodes: Vec<Node> = nodes
            .iter()
            .filter(|n| member_ids.contains(n.id.as_str()))
            .cloned()
            .collect();
        cih_wiki::source_bodies(&body_nodes, repo)
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
    let unresolved_report: Option<String> = unresolved_path
        .and_then(|p| if p.is_file() { std::fs::read_to_string(&p).ok() } else { None });

    let out_dir = out.unwrap_or_else(|| repo.join(".cih").join("wiki"));
    let repo_name = std::fs::canonicalize(repo)
        .unwrap_or_else(|_| repo.to_path_buf())
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let file_dev_map = build_file_dev_map(&nodes, &*feature_of);

    Ok(Some(WikiArtifacts {
        nodes,
        edges,
        wiki_graph,
        community_nodes,
        community_edges,
        community_version,
        graph_version: graph_artifacts.version.0,
        repo_map,
        unresolved_report,
        out_dir,
        repo_name,
        bodies,
        file_dev_map,
        feature_of,
    }))
}

/// Run the LLM-full parallel enrichment phase (deep PO/BA content per community).
fn run_community_full_enrichment(
    community_nodes: &[Node],
    graph: &WikiGraph,
    repo: &Path,
    evidence_corpus: &EvidenceCorpus,
    pool: &rayon::ThreadPool,
    llm: &LlmRunParams<'_>,
    json: bool,
) -> Option<HashMap<String, CommunityLlmFull>> {
    let total_full = community_nodes.len();
    tracing::info!(communities = total_full, "starting LLM full enrichment");

    let mut ui_full = PhaseProgress::new();
    if json {
        ui_full.hide();
    }
    ui_full.start_phase("Deep enrichment (PO/BA)", Some(total_full as u64));

    let results: Vec<(String, Result<CommunityLlmFull>)> = pool.install(|| {
        community_nodes
            .par_iter()
            .map(|comm| {
                ui_full.tick(comm.name.as_str());
                let r = enrich_one_community_full(
                    comm,
                    graph,
                    repo,
                    evidence_corpus,
                    llm.adapter,
                    llm.api_key,
                    llm.model,
                    llm.max_tokens,
                    llm.timeout_secs,
                    llm.retries,
                    llm.language,
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
            Ok(full) => { map.insert(id, full); }
            Err(err) => tracing::warn!(community = %id, error = %err, "LLM full enrichment failed"),
        }
    }
    tracing::info!(enriched = map.len(), "LLM full enrichment complete");
    if map.is_empty() { None } else { Some(map) }
}

/// Run the process-flow enrichment phase (one LLM call per process trace node).
fn run_process_flow_enrichment(
    graph: &WikiGraph,
    llm: &LlmRunParams<'_>,
    json: bool,
) -> HashMap<String, FlowLlmSummary> {
    let total_flows = graph.process_nodes.len();
    if total_flows == 0 {
        return HashMap::new();
    }
    tracing::info!(flows = total_flows, "starting per-flow LLM enrichment");
    let mut ui_flow = PhaseProgress::new();
    if json {
        ui_flow.hide();
    }
    ui_flow.start_phase("Enriching flows", Some(total_flows as u64));
    let mut map = HashMap::new();
    for proc in &graph.process_nodes {
        ui_flow.tick(proc.name.as_str());
        match enrich_one_flow(
            proc,
            graph,
            llm.adapter,
            llm.api_key,
            llm.model,
            llm.max_tokens,
            llm.timeout_secs,
            llm.retries,
            llm.language,
            llm.debug_evidence,
            llm.dry_run,
        ) {
            Ok(summary) => {
                map.insert(proc.id.as_str().to_string(), summary);
                ui_flow.inc_ok();
            }
            Err(err) => {
                tracing::warn!(flow = %proc.id, error = %err, "flow LLM enrichment failed");
                ui_flow.inc_failed();
            }
        }
    }
    ui_flow.finish_phase();
    tracing::info!(enriched = map.len(), "per-flow LLM enrichment complete");
    map
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
        filter_route,
        json,
    } = cfg;
    let repo = repo.as_path();
    let llm_provider = llm_provider.as_str();
    let llm_base_url = llm_base_url.as_str();
    let default_model = match llm_provider {
        "gemini" => "gemini-2.5-flash",
        "anthropic" => "claude-haiku-4-5-20251001",
        "deepseek" => "deepseek-chat",
        _ => "gpt-4o-mini",
    };
    let llm_model_owned;
    let llm_model = if llm_model.is_empty() {
        default_model
    } else {
        llm_model_owned = llm_model;
        llm_model_owned.as_str()
    };
    let wiki_language = wiki_language.as_str();
    let wiki_mode = wiki_mode.as_str();
    let grouping = grouping.as_str();
    // Accept any BCP-47 language tag; we only special-case known languages in
    // prompts but a generic "Write in <lang>" instruction works for any model.
    if wiki_language.is_empty() {
        bail!("--wiki-language must not be empty (e.g. en, vi, ja, fr)");
    }
    let effective_run_llm = run_llm || wiki_mode == "llm-summary" || wiki_mode == "llm-full";
    if !["graph", "llm-summary", "llm-full"].contains(&wiki_mode) {
        bail!("--wiki-mode must be one of: graph, llm-summary, llm-full");
    }
    if !["package", "graph", "llm"].contains(&grouping) {
        bail!("--grouping must be one of: package, graph, llm");
    }
    // llm-full requests 10 JSON fields; 600 tokens (the CLI default) truncates the
    // response mid-object and causes parse failures. Silently raise the floor.
    let llm_max_tokens = if wiki_mode == "llm-full" {
        llm_max_tokens.max(2048)
    } else {
        llm_max_tokens
    };
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

    let art = match load_wiki_artifacts(repo, out, grouping, &filter_community, max_communities, &filter_route)? {
        Some(a) => a,
        None => return Ok(()),
    };
    let WikiArtifacts {
        nodes, edges, wiki_graph, community_nodes, community_edges,
        community_version, graph_version, repo_map, unresolved_report,
        out_dir, repo_name, bodies, file_dev_map, feature_of,
    } = art;
    let repo_name_display = repo_name.clone();

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

    let llm_params: Option<LlmRunParams<'_>> = adapter.as_ref().map(|a| LlmRunParams {
        adapter: a.as_ref(),
        api_key: api_key.as_deref(),
        model: llm_model,
        max_tokens: llm_max_tokens,
        timeout_secs: llm_timeout_secs,
        retries: llm_retries,
        dry_run: llm_dry_run,
        language: wiki_language,
        debug_evidence: llm_debug_evidence,
    });

    let mut llm_info: Option<WikiLlmInfo> = None;
    let mut class_enrichment_store: Option<ClassEnrichmentStore> = None;
    let (llm_summaries, controller_summaries): (
        Option<HashMap<String, CommunityLlmSummary>>,
        Option<HashMap<String, ControllerLlmSummary>>,
    ) = if effective_run_llm {
        let cih_dir = repo.join(".cih");
        let prev_store = if incremental {
            load_class_enrichment(&cih_dir)
        } else {
            ClassEnrichmentStore::default()
        };

        tracing::info!(
            concurrency = concurrency,
            model = llm_model,
            provider = llm_provider,
            "starting class-traversal LLM enrichment"
        );

        let (ctrl_map, comm_map, updated_store) = enrich_classes_for_chains(
            &wiki_graph,
            &nodes,
            repo,
            prev_store,
            adapter.as_ref().unwrap().as_ref(),
            api_key.as_deref(),
            llm_model,
            llm_max_tokens,
            llm_timeout_secs,
            llm_retries,
            wiki_language,
            llm_dry_run || llm_debug_evidence,
            json,
            &filter_route[..],
            concurrency,
        )?;

        tracing::info!(
            classes_in_cache = updated_store.entries.len(),
            comm_summaries = comm_map.len(),
            ctrl_entries = ctrl_map.len(),
            "class-traversal enrichment complete"
        );

        llm_info = Some(WikiLlmInfo {
            provider: llm_provider.to_string(),
            model: llm_model.to_string(),
            language: wiki_language.to_string(),
            evidence_file_count: evidence_corpus.file_count,
            enriched_community_count: comm_map.len(),
            failed_community_count: 0,
            failed_community_ids: vec![],
        });

        class_enrichment_store = Some(updated_store);
        (Some(comm_map), Some(ctrl_map))
    } else {
        (None, None)
    };

    // llm-full: additional richer per-community content for dev + BA pages.
    let llm_full_map: Option<HashMap<String, CommunityLlmFull>> =
        if wiki_mode == "llm-full" && llm_no_call {
            tracing::info!("skipping llm-full enrichment because dry-run/debug mode is enabled");
            None
        } else if wiki_mode == "llm-full" {
            run_community_full_enrichment(
                &community_nodes,
                &wiki_graph,
                repo,
                &evidence_corpus,
                pool.as_ref().unwrap(),
                llm_params.as_ref().unwrap(),
                json,
            )
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
            &graph_version,
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
    let prev_flow_cache: BTreeMap<String, FlowCacheEntry> = if incremental {
        load_wiki_meta(&out_dir)
            .map(|m| m.flow_cache)
            .unwrap_or_default()
    } else {
        BTreeMap::new()
    };
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

        let ui_feat = std::sync::Arc::new(std::sync::Mutex::new(PhaseProgress::new()));
        {
            let mut locked = ui_feat.lock().unwrap();
            if json {
                locked.hide();
            }
            locked.start_phase("Enriching features", Some(active_features.len() as u64));
        }

        // Parallel feature enrichment: one LLM call per feature, independent of each other.
        // Returns (feature_name, summary, ev_hash); None on LLM failure (warning already logged).
        let raw_features: Vec<(String, FeatureLlmSummary, String)> =
            pool.as_ref().unwrap().install(|| {
                active_features
                    .par_iter()
                    .filter_map(|group| {
                        let merged_ev = build_feature_evidence(
                            &group.community_ids,
                            &wiki_graph,
                            repo,
                            &evidence_corpus,
                        );
                        let ev_hash = fnv64(&merged_ev);
                        let citation_map = build_feature_citation_map(
                            &group.community_ids,
                            &wiki_graph,
                            repo,
                            &evidence_corpus,
                            &file_dev_map,
                        );

                        // Cache hit? Post-process cached summaries too so links stay current.
                        if let Some(mut cached) = cached_feature_summary(
                            &group.feature,
                            &ev_hash,
                            prev_meta_for_features.as_ref(),
                        ) {
                            resolve_feature_citations(&mut cached, &citation_map);
                            ui_feat
                                .lock()
                                .unwrap()
                                .tick_skipped(format!("{} (cached)", &group.feature));
                            return Some((group.feature.clone(), cached, ev_hash));
                        }

                        ui_feat.lock().unwrap().tick(group.feature.as_str());
                        tracing::info!(feature = %group.feature, "calling LLM for feature enrichment");
                        match enrich_one_feature(
                            &group.feature,
                            &merged_ev,
                            adapter.as_ref().unwrap().as_ref(),
                            api_key.as_deref(),
                            llm_model,
                            llm_max_tokens,
                            llm_timeout_secs,
                            llm_retries,
                            llm_debug_evidence,
                            llm_dry_run,
                        ) {
                            Ok(mut summary) => {
                                resolve_feature_citations(&mut summary, &citation_map);
                                ui_feat.lock().unwrap().inc_ok();
                                Some((group.feature.clone(), summary, ev_hash))
                            }
                            Err(err) => {
                                tracing::warn!(feature = %group.feature, error = %err, "feature LLM enrichment failed");
                                ui_feat.lock().unwrap().inc_failed();
                                None
                            }
                        }
                    })
                    .collect()
            });

        ui_feat.lock().unwrap().finish_phase();

        let mut map: HashMap<String, FeatureLlmSummary> = HashMap::new();
        for (feature, summary, ev_hash) in raw_features {
            feature_cache_updates.push((feature.clone(), ev_hash, summary.clone()));
            map.insert(feature, summary);
        }

        tracing::info!(features = map.len(), "feature LLM enrichment complete");
        if map.is_empty() {
            None
        } else {
            Some(map)
        }
    } else {
        None
    };

    // Build handler-ID scope for route flow enrichment, filtered to the same features/routes
    // that page generation will use. Without this, all 200+ handlers would be enriched
    // even when --filter-feature or --filter-route limits page output to a single module.
    let route_flow_scope: Option<std::collections::HashSet<String>> = if !filter_feature.is_empty()
    {
        let mut fg = group_communities_by_feature(&wiki_graph);
        retain_matching_feature_groups(&mut fg, &filter_feature);
        let ids: std::collections::HashSet<String> = fg
            .iter()
            .flat_map(|g| g.community_ids.iter())
            .flat_map(|comm_id| {
                wiki_graph
                    .community_routes
                    .get(comm_id.as_str())
                    .into_iter()
                    .flatten()
                    .map(|(handler, _)| handler.id.as_str().to_string())
            })
            .collect();
        Some(ids)
    } else if !filter_route.is_empty() {
        // --filter-route was given: restrict route flow enrichment to handlers whose route
        // path actually matches the filter patterns. Using the community set would include
        // every route in the package, not just the matching ones.
        let ids: std::collections::HashSet<String> = wiki_graph
            .routes
            .iter()
            .filter(|(_, route)| {
                let path = cih_wiki::graph::route_path(route);
                filter_route.iter().any(|f| path.contains(f.as_str()))
            })
            .map(|(handler, _)| handler.id.as_str().to_string())
            .collect();
        if ids.is_empty() { None } else { Some(ids) }
    } else {
        None
    };

    // Per-flow LLM enrichment: one LLM call per process trace.
    let flow_llm_map: Option<HashMap<String, FlowLlmSummary>> = if effective_run_llm && !llm_no_call {
        let map = run_process_flow_enrichment(&wiki_graph, llm_params.as_ref().unwrap(), json);
        if map.is_empty() { None } else { Some(map) }
    } else {
        None
    };

    // Per-route flow enrichment: one LLM call per HTTP handler, generates step descriptions.
    let mut flow_cache_updates: Vec<(String, String, FlowLlmSummary)> = Vec::new();
    let flow_llm_map: Option<HashMap<String, FlowLlmSummary>> = if let Some(mut map) = flow_llm_map
    {
        if effective_run_llm && !llm_no_call {
            let (route_flows, updates) = enrich_route_flows(
                &wiki_graph,
                route_flow_scope.as_ref(),
                adapter.as_ref().unwrap().as_ref(),
                api_key.as_deref(),
                llm_model,
                llm_max_tokens,
                llm_timeout_secs,
                llm_retries,
                wiki_language,
                llm_dry_run,
                &prev_flow_cache,
                concurrency,
            );
            flow_cache_updates = updates;
            map.extend(route_flows);
        }
        Some(map)
    } else if effective_run_llm && !llm_no_call {
        let (route_flows, updates) = enrich_route_flows(
            &wiki_graph,
            route_flow_scope.as_ref(),
            adapter.as_ref().unwrap().as_ref(),
            api_key.as_deref(),
            llm_model,
            llm_max_tokens,
            llm_timeout_secs,
            llm_retries,
            wiki_language,
            llm_dry_run,
            &prev_flow_cache,
            concurrency,
        );
        flow_cache_updates = updates;
        if route_flows.is_empty() {
            None
        } else {
            Some(route_flows)
        }
    } else {
        flow_llm_map
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

    let entrypoints = {
        let path = repo.join(".cih").join("entrypoints.json");
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                serde_json::from_str::<Vec<cih_wiki::EntrypointRecord>>(&raw).unwrap_or_default()
            }
            Err(_) => Vec::new(),
        }
    };

    let input = WikiInput {
        nodes: &nodes,
        edges: &edges,
        community_nodes: &community_nodes,
        community_edges: &community_edges,
        repo_name,
        graph_version,
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
        flow_llm_summaries: flow_llm_map,
        grouping: grouping.to_string(),
        filter_feature,
        bodies,
        feature_of,
        entrypoints,
    };

    tracing::info!(out_dir = %out_dir.display(), "generating wiki pages");
    let mut ui_gen = crate::ui::PhaseProgress::new();
    if json {
        ui_gen.hide();
    }
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

    persist_wiki_meta_caches(&out_dir, &[], &feature_cache_updates, &flow_cache_updates)?;

    if let Some(store) = &class_enrichment_store {
        let cih_dir = repo.join(".cih");
        if let Err(e) = save_class_enrichment(&cih_dir, store) {
            tracing::warn!(error = %e, "failed to save class enrichment cache");
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
        crate::ui::print_header("Wiki", &repo_name_display, None);
        crate::ui::print_row("Pages", &outcome.page_count.to_string());
        crate::ui::print_row(
            "Communities",
            &format!(
                "{}  routes {}",
                outcome.community_count, outcome.route_count
            ),
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
        if full.po_summary.is_empty() && full.ba_process_overview.is_empty() {
            None
        } else {
            Some(full)
        }
    };
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        if let Some(r) = try_extract(&val) {
            return Ok(r);
        }
    }
    if let (Some(s), Some(e)) = (text.find('{'), text.rfind('}')) {
        if s < e {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text[s..=e]) {
                if let Some(r) = try_extract(&val) {
                    return Ok(r);
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
    evidence_corpus: &crate::llm::evidence::EvidenceCorpus,
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    retries: u32,
    language: &str,
) -> Result<CommunityLlmFull> {
    use crate::llm::evidence::build_evidence_pack;
    let evidence_pack = build_evidence_pack(Some(repo), graph, community, evidence_corpus);
    let evidence = evidence_pack.render();
    let system = format!(
        "You are a code documentation assistant. Write only from the provided evidence. \
         Do not invent behavior not in the evidence.{}",
        if language != "en" {
            format!(" Write all documentation in language: {}.", language)
        } else {
            String::new()
        }
    );
    let user = build_full_prompt(&community.name, &evidence);
    let request = LlmRequest {
        system,
        user,
        model: model.to_string(),
        max_tokens,
        timeout_secs,
    };
    let jitter: u64 = community
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
                    let delay = backoff_ms(attempt, jitter.wrapping_add(attempt as u64));
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

/// Walk call chains from every route handler, enrich each unique class once,
/// cache results in `.cih/class-enrichment.json`.
///
/// Returns:
/// - `ctrl_map`: simple class name → `ControllerLlmSummary` (covers ALL classes in chains,
///   not just controllers; `generate_wiki` resolves method descriptions from this map)
/// - `comm_map`: community_id → synthesized `CommunityLlmSummary` (aggregated class summaries)
/// - `ClassEnrichmentStore`: updated cache to persist after wiki generation
pub fn enrich_classes_for_chains(
    wiki_graph: &WikiGraph,
    all_nodes: &[cih_core::Node],
    repo: &Path,
    prev_store: ClassEnrichmentStore,
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    retries: u32,
    language: &str,
    dry_run: bool,
    json_output: bool,
    filter_route: &[String],
    concurrency: usize,
) -> Result<(
    HashMap<String, ControllerLlmSummary>,
    HashMap<String, CommunityLlmSummary>,
    ClassEnrichmentStore,
)> {
    use std::collections::BTreeMap;

    // Collect method IDs per FQCN by walking filtered route handler call chains.
    let mut class_methods: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for routes in wiki_graph.routes_by_controller.values() {
        for (handler, route) in routes {
            if !filter_route.is_empty() && {
                let path = cih_wiki::graph::route_path(route);
                !filter_route.iter().any(|f| path.contains(f.as_str()))
            }
            {
                continue;
            }
            let chain = wiki_graph.build_call_chain(handler.id.as_str(), 4);
            for method_id in chain {
                let fqcn = method_id
                    .strip_prefix("Method:")
                    .or_else(|| method_id.strip_prefix("Constructor:"))
                    .and_then(|s| s.split('#').next())
                    .unwrap_or("")
                    .to_string();
                if fqcn.is_empty() {
                    continue;
                }
                let methods = class_methods.entry(fqcn).or_default();
                if !methods.contains(&method_id) {
                    methods.push(method_id);
                }
            }
        }
    }

    let total = class_methods.len();
    tracing::info!(classes = total, "class-traversal: enriching {} unique classes", total);

    // Quick node lookup for source body reading.
    let node_by_id: HashMap<&str, &cih_core::Node> =
        all_nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    // Snapshot prev entries for cache checks (read-only, shared across threads).
    let prev_entries = prev_store.entries.clone();

    let ui = std::sync::Arc::new(std::sync::Mutex::new(PhaseProgress::new()));
    {
        let mut locked = ui.lock().unwrap();
        if json_output {
            locked.hide();
        }
        locked.start_phase("Enriching classes", Some(total as u64));
    }

    let effective_concurrency = concurrency.max(1);
    let class_list: Vec<(&String, &Vec<String>)> = class_methods.iter().collect();

    // Build per-class entries in parallel; each entry is (fqcn, ClassCacheEntry | None-if-cached).
    let new_entries: Vec<(String, ClassCacheEntry)> = {
        use rayon::prelude::*;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(effective_concurrency)
            .build()
            .unwrap_or_else(|_| rayon::ThreadPoolBuilder::new().build().unwrap());

        pool.install(|| {
            class_list
                .par_iter()
                .filter_map(|(fqcn, method_ids)| {
                    let simple_name = fqcn.rsplit('.').next().unwrap_or(fqcn.as_str());

                    let method_nodes: Vec<cih_core::Node> = method_ids
                        .iter()
                        .filter_map(|id| node_by_id.get(id.as_str()).copied().cloned())
                        .collect();

                    let bodies = cih_wiki::source_bodies(&method_nodes, repo);

                    let mut sorted_bodies: Vec<(&str, &str)> = method_ids
                        .iter()
                        .filter_map(|id| {
                            bodies
                                .get(id.as_str())
                                .map(|b| (id.as_str(), b.stripped.as_str()))
                        })
                        .collect();
                    sorted_bodies.sort_by_key(|(id, _)| *id);

                    let combined = sorted_bodies
                        .iter()
                        .map(|(_, b)| *b)
                        .collect::<Vec<_>>()
                        .join("\n---\n");
                    let content_hash = fnv64(&combined);

                    // Cache hit — same source, skip LLM call.
                    if let Some(cached) = prev_entries.get(fqcn.as_str()) {
                        if cached.content_hash == content_hash {
                            ui.lock().unwrap().tick_skipped(format!("{} (cached)", simple_name));
                            return None;
                        }
                    }

                    ui.lock().unwrap().tick(simple_name);

                    let entry = if dry_run {
                        println!("--- [dry-run] class: {} ---", fqcn);
                        ClassCacheEntry {
                            content_hash,
                            method_descriptions: method_ids
                                .iter()
                                .filter_map(|id| {
                                    let m = id
                                        .split('#')
                                        .nth(1)
                                        .and_then(|x| x.split('/').next())?;
                                    Some((m.to_string(), format!("[dry-run] {}", m)))
                                })
                                .collect(),
                            class_summary: format!("[dry-run] {}", simple_name),
                        }
                    } else {
                        let system = build_class_system_prompt(language);
                        let user = build_class_enrich_prompt(fqcn, &sorted_bodies);
                        let request = LlmRequest {
                            system,
                            user,
                            model: model.to_string(),
                            max_tokens: max_tokens.max(2000),
                            timeout_secs,
                        };
                        let jitter: u64 = fqcn
                            .bytes()
                            .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
                        let mut ok = None;
                        for attempt in 0..=(retries as usize) {
                            match adapter
                                .call(api_key, &request)
                                .and_then(|r| parse_class_enrich_response(&r.text))
                            {
                                Ok((summary, method_descs)) => {
                                    ok = Some(ClassCacheEntry {
                                        content_hash: content_hash.clone(),
                                        method_descriptions: method_descs,
                                        class_summary: summary,
                                    });
                                    break;
                                }
                                Err(err) => {
                                    if attempt < retries as usize {
                                        let delay = backoff_ms(
                                            attempt,
                                            jitter.wrapping_add(attempt as u64),
                                        );
                                        std::thread::sleep(std::time::Duration::from_millis(
                                            delay,
                                        ));
                                    } else {
                                        tracing::warn!(
                                            class = %fqcn,
                                            error = %err,
                                            "class enrichment failed"
                                        );
                                    }
                                }
                            }
                        }
                        ok.unwrap_or_else(|| ClassCacheEntry {
                            content_hash,
                            method_descriptions: HashMap::new(),
                            class_summary: String::new(),
                        })
                    };

                    Some(((*fqcn).clone(), entry))
                })
                .collect()
        })
    };

    // Merge parallel results into the entry map (start from prev_entries for cached ones).
    let mut updated_entries: BTreeMap<String, ClassCacheEntry> = prev_entries;
    for (fqcn, entry) in new_entries {
        updated_entries.insert(fqcn, entry);
    }

    ui.lock().unwrap().finish_phase();

    // Build ControllerLlmSummary keyed by simple class name — covers all classes in chains.
    let mut ctrl_map: HashMap<String, ControllerLlmSummary> = HashMap::new();
    for (fqcn, _) in &class_methods {
        let simple_name = fqcn.rsplit('.').next().unwrap_or(fqcn.as_str()).to_string();
        if let Some(entry) = updated_entries.get(fqcn.as_str()) {
            ctrl_map.insert(
                simple_name,
                ControllerLlmSummary {
                    description: entry.class_summary.clone(),
                    feature: None,
                    method_descriptions: entry.method_descriptions.clone(),
                },
            );
        }
    }

    // Synthesize CommunityLlmSummary: aggregate class summaries by community.
    let mut comm_texts: HashMap<String, Vec<String>> = HashMap::new();
    for (fqcn, method_ids) in &class_methods {
        let Some(entry) = updated_entries.get(fqcn.as_str()) else {
            continue;
        };
        if entry.class_summary.is_empty() {
            continue;
        }
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for mid in method_ids {
            if let Some(comm_id) = wiki_graph.community_by_member.get(mid.as_str()) {
                if seen.insert(comm_id.as_str()) {
                    comm_texts
                        .entry(comm_id.clone())
                        .or_default()
                        .push(entry.class_summary.clone());
                }
            }
        }
    }
    let comm_map: HashMap<String, CommunityLlmSummary> = comm_texts
        .into_iter()
        .map(|(id, summaries)| {
            let text = summaries.join(" ");
            (
                id,
                CommunityLlmSummary {
                    po: text.clone(),
                    ba: text,
                    dev: String::new(),
                },
            )
        })
        .collect();

    Ok((
        ctrl_map,
        comm_map,
        ClassEnrichmentStore {
            schema_version: 1,
            entries: updated_entries,
        },
    ))
}

fn build_class_system_prompt(language: &str) -> String {
    let mut s = String::from(
        "You are a code documentation assistant. Describe Java class methods in one sentence \
         each for a business analyst. Return JSON only. Do not invent behavior. \
         Start each method description with an action verb. \
         Do not mention the class name, method name, or arity (e.g. /2()) in the description.",
    );
    if language != "en" {
        s.push_str(&format!(" Write all descriptions in language: {}.", language));
    }
    s
}

fn build_class_enrich_prompt(fqcn: &str, bodies: &[(&str, &str)]) -> String {
    let simple = fqcn.rsplit('.').next().unwrap_or(fqcn);
    let mut s = format!("Class: {simple}\n\nMethods:\n");
    for (i, (method_id, body)) in bodies.iter().enumerate() {
        let method_name = method_id
            .split('#')
            .nth(1)
            .and_then(|x| x.split('/').next())
            .unwrap_or("unknown");
        let truncated = if body.len() > 600 { &body[..600] } else { body };
        s.push_str(&format!("{}. {}\n{}\n\n", i + 1, method_name, truncated));
    }
    s.push_str(
        "Return exactly this JSON:\n\
         {\n\
           \"summary\": \"one paragraph: what this class does in the system\",\n\
           \"methods\": {\n\
             \"methodName\": \"Validates the request payload and delegates to the write service.\"\n\
           }\n\
         }\n\
         Each method value must start with a verb and must not repeat the class or method name.\n\
         Output only the JSON object.",
    );
    s
}

/// Scan for `"summary": "..."` in a truncated/invalid JSON string without a full parser.
/// Returns the summary value if found, None otherwise.
fn extract_summary_from_partial(text: &str) -> Option<String> {
    let key = "\"summary\":";
    let start = text.find(key)?;
    let after_key = text[start + key.len()..].trim_start();
    if !after_key.starts_with('"') {
        return None;
    }
    let s = &after_key[1..]; // skip opening quote
    let mut summary = String::new();
    let mut chars = s.chars().peekable();
    loop {
        match chars.next() {
            Some('\\') => {
                // Escaped character — include it decoded
                match chars.next() {
                    Some('n') => summary.push('\n'),
                    Some('t') => summary.push('\t'),
                    Some(c) => summary.push(c),
                    None => break,
                }
            }
            Some('"') => break, // closing quote
            Some(c) => summary.push(c),
            None => break, // truncated mid-string, use what we got
        }
    }
    if summary.trim().is_empty() {
        None
    } else {
        Some(summary.trim().to_string())
    }
}

fn parse_class_enrich_response(text: &str) -> Result<(String, HashMap<String, String>)> {
    let cleaned = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let extract =
        |val: &serde_json::Value| -> Option<(String, HashMap<String, String>)> {
            let summary = val["summary"].as_str().unwrap_or("").to_string();
            let methods: HashMap<String, String> = val["methods"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            if summary.is_empty() && methods.is_empty() {
                None
            } else {
                Some((summary, methods))
            }
        };

    if let Ok(val) = serde_json::from_str::<serde_json::Value>(cleaned) {
        if let Some(r) = extract(&val) {
            return Ok(r);
        }
    }
    if let (Some(s), Some(e)) = (cleaned.find('{'), cleaned.rfind('}')) {
        if s < e {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&cleaned[s..=e]) {
                if let Some(r) = extract(&val) {
                    return Ok(r);
                }
            }
        }
    }
    // Fallback: truncated JSON — try to extract at least the summary by string scanning.
    if let Some(summary) = extract_summary_from_partial(cleaned) {
        tracing::debug!(
            "class enrichment: partial JSON recovered (summary only), methods lost"
        );
        return Ok((summary, HashMap::new()));
    }
    bail!(
        "failed to extract class JSON from LLM response: {:?}",
        &text[..text.len().min(200)]
    )
}

fn persist_wiki_meta_caches(
    out_dir: &Path,
    community_updates: &[(String, String, CommunityLlmSummary)],
    feature_updates: &[(String, String, FeatureLlmSummary)],
    flow_updates: &[(String, String, FlowLlmSummary)],
) -> Result<()> {
    if community_updates.is_empty() && feature_updates.is_empty() && flow_updates.is_empty() {
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

    for (handler_id, ev_hash, summary) in flow_updates {
        meta.flow_cache.insert(
            handler_id.clone(),
            FlowCacheEntry {
                evidence_hash: ev_hash.clone(),
                summary: summary.clone(),
            },
        );
    }

    let json = serde_json::to_string_pretty(&meta).context("failed to serialize wiki metadata")?;
    std::fs::write(&meta_path, json)
        .with_context(|| format!("failed to write {}", meta_path.display()))?;

    Ok(())
}

/// Retain only communities that have at least one route whose path starts with
/// or contains one of the given patterns (case-insensitive). When `patterns` is
/// empty the full list is returned unchanged.
fn filter_communities_by_route(
    mut communities: Vec<cih_core::Node>,
    graph: &WikiGraph,
    patterns: &[String],
) -> Vec<cih_core::Node> {
    if patterns.is_empty() {
        return communities;
    }
    let patterns_lower: Vec<String> = patterns.iter().map(|p| p.to_lowercase()).collect();
    let before = communities.len();
    communities.retain(|n| {
        let comm_id = n.id.as_str();
        graph
            .community_routes
            .get(comm_id)
            .map(|routes| {
                routes.iter().any(|(_, route)| {
                    let path = route_path(route).to_lowercase();
                    patterns_lower
                        .iter()
                        .any(|pat| path.starts_with(pat.as_str()) || path.contains(pat.as_str()))
                })
            })
            .unwrap_or(false)
    });
    if communities.len() != before {
        tracing::info!(
            before = before,
            after = communities.len(),
            patterns = ?patterns,
            "route filter applied"
        );
        eprintln!(
            "info: --filter-route matched {} of {} communities",
            communities.len(),
            before
        );
    }
    communities
}

/// Build evidence text for a single process trace (call chain).
/// Format: triggering route if any, then per-step context, capped at 2000 chars.
fn build_flow_evidence(process_node: &cih_core::Node, graph: &WikiGraph) -> String {
    const MAX_FLOW_EVIDENCE: usize = 2_000;
    let proc_id = process_node.id.as_str();
    let mut out = String::new();

    // Triggering route from props["route"]
    if let Some(route) = process_node
        .props
        .as_ref()
        .and_then(|p| p.get("route"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        out.push_str(&format!("Triggered by: {}\n\n", route));
    }

    let Some(steps) = graph.process_steps.get(proc_id) else {
        return out;
    };

    out.push_str("Steps:\n");
    for step in steps {
        let method_id = step.symbol.id.as_str();

        // Derive class name from method id (Method:fqcn#name → SimpleClass)
        let class_name = method_id
            .split_once('#')
            .map(|(prefix, _)| {
                prefix
                    .trim_start_matches("Method:")
                    .trim_start_matches("Constructor:")
                    .rsplit('.')
                    .next()
                    .unwrap_or(prefix)
            })
            .unwrap_or("");

        // Stereotype of the class
        let stereotype = method_id
            .split_once('#')
            .and_then(|(prefix, _)| {
                let fqcn = prefix
                    .trim_start_matches("Method:")
                    .trim_start_matches("Constructor:");
                let cls_id = format!("Class:{}", fqcn);
                graph
                    .nodes_by_id
                    .get(&cls_id)
                    .and_then(cih_wiki::graph::node_stereotype)
            })
            .unwrap_or("");

        // Outgoing calls (up to 4)
        let empty_calls: Vec<String> = Vec::new();
        let calls: Vec<&str> = graph
            .calls_out
            .get(method_id)
            .unwrap_or(&empty_calls)
            .iter()
            .take(4)
            .filter_map(|cid| graph.nodes_by_id.get(cid).map(|n| n.name.as_str()))
            .collect();

        // DB tables (reads/writes)
        let mut tables: Vec<String> = Vec::new();
        if let Some(qids) = graph.executes_query.get(method_id) {
            for qid in qids.iter().take(4) {
                for tid in graph
                    .query_reads_table
                    .get(qid.as_str())
                    .into_iter()
                    .flatten()
                    .take(2)
                {
                    let name = tid.strip_prefix("DbTable:").unwrap_or(tid);
                    tables.push(format!("{}(r)", name));
                }
                for tid in graph
                    .query_writes_table
                    .get(qid.as_str())
                    .into_iter()
                    .flatten()
                    .take(2)
                {
                    let name = tid.strip_prefix("DbTable:").unwrap_or(tid);
                    tables.push(format!("{}(w)", name));
                }
            }
        }

        let mut line = format!(
            "[{}] {} — {} ({})",
            step.step_number,
            step.symbol.name,
            class_name,
            if stereotype.is_empty() {
                "?"
            } else {
                stereotype
            }
        );
        if !calls.is_empty() {
            line.push_str(&format!(" | calls: {}", calls.join(", ")));
        }
        if !tables.is_empty() {
            line.push_str(&format!(" | tables: {}", tables.join(", ")));
        }
        line.push('\n');

        if out.len() + line.len() > MAX_FLOW_EVIDENCE {
            break;
        }
        out.push_str(&line);
    }

    out
}

fn chain_steps_text(chain: &[String], graph: &WikiGraph) -> String {
    chain
        .iter()
        .enumerate()
        .map(|(i, mid)| {
            let (class_name, method_name) = mid
                .split_once('#')
                .map(|(prefix, method)| {
                    let cls = prefix
                        .trim_start_matches("Method:")
                        .trim_start_matches("Constructor:")
                        .rsplit('.')
                        .next()
                        .unwrap_or(prefix);
                    (cls, method)
                })
                .unwrap_or(("?", mid.as_str()));
            let cls_id = cih_wiki::pages::api_flow::class_id_from_method_id(mid.as_str(), graph);
            let stereotype = graph
                .nodes_by_id
                .get(cls_id.as_str())
                .and_then(cih_wiki::graph::node_stereotype)
                .unwrap_or("?");
            format!(
                "[{}] {}.{}() ({})",
                i + 1,
                class_name,
                method_name,
                stereotype
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn enrich_route_flows(
    graph: &WikiGraph,
    scope: Option<&std::collections::HashSet<String>>,
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    retries: u32,
    language: &str,
    dry_run: bool,
    flow_cache: &BTreeMap<String, FlowCacheEntry>,
    concurrency: usize,
) -> (HashMap<String, FlowLlmSummary>, Vec<(String, String, FlowLlmSummary)>) {
    let handlers: Vec<(String, String)> = graph
        .routes_by_controller
        .values()
        .flat_map(|routes| {
            routes
                .iter()
                .map(|(handler, _route)| (handler.id.as_str().to_string(), handler.name.clone()))
        })
        .filter(|(id, _)| scope.map_or(true, |s| s.contains(id.as_str())))
        .collect();

    if handlers.is_empty() {
        return (HashMap::new(), Vec::new());
    }

    let ui = std::sync::Arc::new(std::sync::Mutex::new(PhaseProgress::new()));
    ui.lock()
        .unwrap()
        .start_phase("Enriching route flows", Some(handlers.len() as u64));

    let effective_concurrency = concurrency.max(1);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(effective_concurrency)
        .build()
        .unwrap_or_else(|_| rayon::ThreadPoolBuilder::new().build().unwrap());

    // Each item: (handler_id, summary, Option<evidence_hash>)
    // evidence_hash is Some only for fresh LLM results; None for cache hits or dry-run.
    let raw: Vec<(String, FlowLlmSummary, Option<String>)> = pool.install(|| {
        handlers
            .par_iter()
            .filter_map(|(handler_id, handler_name)| {
                let chain = graph.build_call_chain(handler_id.as_str(), 4);
                if chain.is_empty() {
                    ui.lock().unwrap().inc_ok();
                    return None;
                }
                let step_count = chain.len();
                let steps_text = chain_steps_text(&chain, graph);
                let evidence_hash = fnv64(&steps_text);

                // Cache hit: chain unchanged since last run.
                if let Some(cached) = flow_cache.get(handler_id.as_str()) {
                    if cached.evidence_hash == evidence_hash {
                        ui.lock().unwrap().inc_ok();
                        return Some((handler_id.clone(), cached.summary.clone(), None));
                    }
                }

                ui.lock().unwrap().tick(handler_name.as_str());

                if dry_run {
                    let summary = FlowLlmSummary {
                        narrative: format!("[dry-run] {}", handler_name),
                        business_impact: String::new(),
                        step_descriptions: vec!["[dry-run]".into(); step_count],
                    };
                    ui.lock().unwrap().inc_ok();
                    return Some((handler_id.clone(), summary, None));
                }

                let mut system = String::from(
                    "You are a code documentation assistant. Describe this HTTP request flow \
                     based solely on the provided call chain. Do not invent behavior not shown. \
                     Each step description must start with an action verb and must not repeat \
                     the class name, method name, or arity notation (e.g. /2()).",
                );
                if language != "en" {
                    system.push_str(&format!(
                        " Write all documentation in language: {}.",
                        language
                    ));
                }
                let user = format!(
                    r#"HTTP handler: "{name}"

Call chain ({step_count} steps):
{steps}

Respond ONLY with this JSON object (no extra commentary):
{{
  "narrative": "<2-3 sentences describing this request flow for a business analyst>",
  "business_impact": "<1-2 sentences describing the business value for a product owner>",
  "step_descriptions": [<one quoted sentence per step, {step_count} total>]
}}"#,
                    name = handler_name,
                    step_count = step_count,
                    steps = steps_text,
                );
                let effective_max_tokens = route_flow_token_budget(step_count, max_tokens);
                let req = LlmRequest {
                    system,
                    user,
                    model: model.to_string(),
                    max_tokens: effective_max_tokens,
                    timeout_secs,
                };
                let jitter_seed: u64 = handler_id
                    .as_str()
                    .bytes()
                    .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));

                let mut last_err = None;
                for attempt in 0..=(retries as usize) {
                    match adapter
                        .call(api_key, &req)
                        .and_then(|r| parse_flow_summary(&r.text, step_count))
                    {
                        Ok(summary) => {
                            ui.lock().unwrap().inc_ok();
                            return Some((
                                handler_id.clone(),
                                summary,
                                Some(evidence_hash),
                            ));
                        }
                        Err(err) => {
                            if attempt < retries as usize {
                                let delay =
                                    backoff_ms(attempt, jitter_seed.wrapping_add(attempt as u64));
                                tracing::debug!(
                                    attempt = attempt + 1,
                                    delay_ms = delay,
                                    error = %err,
                                    "route flow LLM call failed, retrying"
                                );
                                std::thread::sleep(std::time::Duration::from_millis(delay));
                            }
                            last_err = Some(err);
                        }
                    }
                }
                tracing::warn!(
                    handler = %handler_id,
                    error = %last_err.unwrap(),
                    "route flow LLM enrichment failed"
                );
                ui.lock().unwrap().inc_failed();
                None
            })
            .collect()
    });

    ui.lock().unwrap().finish_phase();

    let mut result = HashMap::with_capacity(raw.len());
    let mut cache_updates = Vec::new();
    for (handler_id, summary, maybe_hash) in raw {
        if let Some(ev_hash) = maybe_hash {
            cache_updates.push((handler_id.clone(), ev_hash, summary.clone()));
        }
        result.insert(handler_id, summary);
    }
    (result, cache_updates)
}

fn enrich_one_flow(
    process_node: &cih_core::Node,
    graph: &WikiGraph,
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    retries: u32,
    language: &str,
    debug_evidence: bool,
    dry_run: bool,
) -> Result<FlowLlmSummary> {
    let evidence = build_flow_evidence(process_node, graph);
    let step_count = graph
        .process_steps
        .get(process_node.id.as_str())
        .map(|s| s.len())
        .unwrap_or(0);

    let mut system = String::from(
        "You are a code documentation assistant. Describe this business process \
         based solely on the provided evidence. Do not invent behavior not shown.",
    );
    if language != "en" {
        system.push_str(&format!(
            " Write all documentation in language: {}.",
            language
        ));
    }
    let evidence_str = if evidence.trim().is_empty() {
        "none"
    } else {
        &evidence
    };
    let user = format!(
        r#"Process: "{name}"

{evidence}

Respond ONLY with this JSON object (no extra commentary):
{{
  "narrative": "<2-3 sentences describing this flow for a business analyst>",
  "business_impact": "<1-2 sentences describing the business value for a product owner>",
  "step_descriptions": [<one quoted sentence per step, {step_count} total>]
}}"#,
        name = process_node.name,
        evidence = evidence_str,
        step_count = step_count,
    );

    if debug_evidence {
        println!("--- [flow evidence] process: {} ---", process_node.name);
        println!("{}", evidence_str);
        return Ok(FlowLlmSummary {
            narrative: format!("[debug-evidence] {}", process_node.name),
            business_impact: String::new(),
            step_descriptions: vec!["[debug]".into(); step_count],
        });
    }
    if dry_run {
        println!("--- [dry-run] flow: {} ---", process_node.name);
        println!("System:\n{}\n", system);
        println!("User:\n{}", user);
        return Ok(FlowLlmSummary {
            narrative: format!("[dry-run] {}", process_node.name),
            business_impact: String::new(),
            step_descriptions: vec!["[dry-run]".into(); step_count],
        });
    }

    let req = LlmRequest {
        system,
        user,
        model: model.to_string(),
        max_tokens,
        timeout_secs,
    };
    let jitter_seed: u64 = process_node
        .id
        .as_str()
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    let mut last_err = None;
    for attempt in 0..=(retries as usize) {
        match adapter
            .call(api_key, &req)
            .and_then(|r| parse_flow_summary(&r.text, step_count))
        {
            Ok(summary) => return Ok(summary),
            Err(err) => {
                if attempt < retries as usize {
                    let delay = backoff_ms(attempt, jitter_seed.wrapping_add(attempt as u64));
                    tracing::debug!(attempt = attempt + 1, delay_ms = delay, error = %err, "flow LLM call failed, retrying");
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

/// Token budget for route-flow enrichment: ~100 tokens per step for step_descriptions
/// plus ~500 overhead for narrative/business_impact/JSON framing; floor at 2 000.
fn route_flow_token_budget(step_count: usize, base: u32) -> u32 {
    base.max(step_count as u32 * 100 + 500).max(2000)
}

/// Scan truncated JSON text and extract whatever flow fields are present.
/// Mirrors `extract_summary_from_partial` used for class enrichment.
fn extract_flow_partial(text: &str, step_count: usize) -> Option<FlowLlmSummary> {
    fn extract_string_value(text: &str, key: &str) -> Option<String> {
        let needle = format!("\"{}\":", key);
        let start = text.find(needle.as_str())?;
        let after = text[start + needle.len()..].trim_start();
        if !after.starts_with('"') {
            return None;
        }
        let mut out = String::new();
        let mut chars = after[1..].chars().peekable();
        loop {
            match chars.next() {
                Some('\\') => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(c) => out.push(c),
                    None => break,
                },
                Some('"') => break,
                Some(c) => out.push(c),
                None => break,
            }
        }
        if out.trim().is_empty() { None } else { Some(out.trim().to_string()) }
    }

    let narrative = extract_string_value(text, "narrative").unwrap_or_default();
    let business_impact = extract_string_value(text, "business_impact").unwrap_or_default();

    if narrative.is_empty() && business_impact.is_empty() {
        return None;
    }

    let mut descs = Vec::new();
    if let Some(arr_start) = text.find("\"step_descriptions\"") {
        let after = &text[arr_start..];
        if let Some(bracket) = after.find('[') {
            let content = &after[bracket + 1..];
            let mut in_str = false;
            let mut current = String::new();
            let mut chars = content.chars();
            loop {
                match chars.next() {
                    None | Some(']') => {
                        if in_str && !current.trim().is_empty() {
                            descs.push(current.trim().to_string());
                        }
                        break;
                    }
                    Some('"') if !in_str => { in_str = true; }
                    Some('"') if in_str => {
                        descs.push(current.trim().to_string());
                        current = String::new();
                        in_str = false;
                    }
                    Some('\\') if in_str => {
                        match chars.next() {
                            Some('n') => current.push('\n'),
                            Some('t') => current.push('\t'),
                            Some(c) => current.push(c),
                            None => break,
                        }
                    }
                    Some(c) if in_str => current.push(c),
                    _ => {}
                }
            }
        }
    }
    descs.resize(step_count, String::new());

    Some(FlowLlmSummary { narrative, business_impact, step_descriptions: descs })
}

pub fn parse_flow_summary(text: &str, step_count: usize) -> Result<FlowLlmSummary> {
    let stripped = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let json_str = if let (Some(s), Some(e)) = (stripped.find('{'), stripped.rfind('}')) {
        if s <= e { &stripped[s..=e] } else { stripped }
    } else {
        stripped
    };
    match serde_json::from_str::<serde_json::Value>(json_str) {
        Ok(val) => {
            let narrative = val["narrative"].as_str().unwrap_or("").to_string();
            let business_impact = val["business_impact"].as_str().unwrap_or("").to_string();
            let mut descs: Vec<String> = val["step_descriptions"]
                .as_array()
                .map(|arr| arr.iter().map(|v| v.as_str().unwrap_or("").to_string()).collect())
                .unwrap_or_default();
            descs.resize(step_count, String::new());

            if narrative.is_empty() && business_impact.is_empty() && descs.iter().all(|s| s.is_empty()) {
                bail!("flow LLM response did not contain any expected fields");
            }
            Ok(FlowLlmSummary { narrative, business_impact, step_descriptions: descs })
        }
        Err(parse_err) => {
            // Truncated JSON — try partial recovery before giving up.
            if let Some(partial) = extract_flow_partial(stripped, step_count) {
                tracing::debug!("flow enrichment: partial JSON recovered (narrative/impact only)");
                return Ok(partial);
            }
            bail!(
                "failed to parse flow LLM response: {parse_err}: {:?}",
                &text[..text.len().min(200)]
            )
        }
    }
}

pub fn retain_matching_feature_groups(
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
pub fn build_feature_evidence(
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
pub fn enrich_one_feature(
    feature: &str,
    evidence: &str,
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    retries: u32,
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
    let jitter_seed: u64 = feature
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    let mut last_err = None;
    for attempt in 0..=(retries as usize) {
        match adapter
            .call(api_key, &req)
            .and_then(|r| parse_feature_summary(&r.text))
        {
            Ok(summary) => return Ok(summary),
            Err(err) => {
                if attempt < retries as usize {
                    let delay = backoff_ms(attempt, jitter_seed.wrapping_add(attempt as u64));
                    tracing::debug!(attempt = attempt + 1, delay_ms = delay, error = %err, "feature LLM call failed, retrying");
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

fn build_feature_system_prompt() -> String {
    "You are a software architect writing business documentation from code evidence.\n\
     Write only from the provided evidence. Cite evidence IDs exactly as shown, like [C1-R1],[C1-P1],[C2-B1].\n\
     Do not invent behavior not in the evidence."
        .to_string()
}

pub fn build_feature_user_prompt(feature: &str, evidence: &str) -> String {
    format!(
        r#"You are writing feature-level documentation for the "{feature}" module.

Evidence (grouped by community):
{evidence}

Respond ONLY with a JSON object:
{{
  "po_overview": "<3-5 sentences of plain-language business overview>",
  "po_capabilities": "<bullet list of business capabilities, one per line starting with - >",
  "ba_process_overview": "<3-5 sentences describing business processes and flows>",
  "ba_business_rules": "<key business rules or invariants, one per line starting with - >"
}}"#
    )
}

pub fn parse_feature_summary(text: &str) -> Result<FeatureLlmSummary> {
    // Some models (e.g. Gemini) wrap JSON in ```json ... ``` fences.
    let stripped = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let json_str = if let (Some(start), Some(end)) = (stripped.find('{'), stripped.rfind('}')) {
        if start <= end {
            &stripped[start..=end]
        } else {
            stripped
        }
    } else {
        stripped
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

pub fn cached_feature_summary(
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

/// Extract the first meaningful (non-generic) path segment from a route pattern,
/// using the same skip-list as community detection so that the result matches
/// the values stored in `props["route_prefixes"]`.
fn first_meaningful_route_seg(path: &str) -> Option<String> {
    const GENERIC: &[&str] = &[
        "api", "apis", "rest", "internal", "external", "service", "services", "common", "shared",
        "core", "app", "apps", "admin", "pos", "public", "private",
    ];
    for part in path.split('/') {
        let part = part.trim();
        if part.is_empty() || part.starts_with('{') || part.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let mut chars = part.chars();
        if matches!(chars.next(), Some('v') | Some('V')) && chars.all(|c| c.is_ascii_digit()) {
            continue;
        }
        let lower = part.to_lowercase();
        if GENERIC.contains(&lower.as_str()) {
            continue;
        }
        return Some(lower);
    }
    None
}

/// Returns true if a community's stored `route_prefixes` (from discover) overlap with
/// any of the `--filter-route` patterns. Used as a fast pre-filter before loading the
/// main graph; false-positives are acceptable since the precise filter re-runs later.
/// Returns true when props are absent (can't pre-filter → keep).
pub fn community_matches_route_prefix(community: &Node, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let Some(props) = &community.props else {
        return true;
    };
    let Some(arr) = props.get("route_prefixes").and_then(|v| v.as_array()) else {
        return true;
    };
    if arr.is_empty() {
        return false;
    }
    let prefixes: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
    patterns.iter().any(|pat| {
        let Some(pat_seg) = first_meaningful_route_seg(pat) else {
            return true; // can't parse pattern segment → keep (safe false-positive)
        };
        prefixes.iter().any(|p| {
            let p_lower = p.to_lowercase();
            p_lower == pat_seg
                || p_lower.contains(pat_seg.as_str())
                || pat_seg.contains(p_lower.as_str())
        })
    })
}

/// Post-processes all text fields in a `FeatureLlmSummary`, replacing bare
/// citation IDs with Markdown links wherever the citation map has a URL.
fn resolve_feature_citations(
    summary: &mut FeatureLlmSummary,
    citation_map: &HashMap<String, String>,
) {
    if citation_map.is_empty() {
        return;
    }
    summary.po_overview = replace_citations(&summary.po_overview, citation_map);
    summary.po_capabilities = replace_citations(&summary.po_capabilities, citation_map);
    summary.ba_process_overview = replace_citations(&summary.ba_process_overview, citation_map);
    summary.ba_business_rules = replace_citations(&summary.ba_business_rules, citation_map);
}

/// Maps every class/interface source file to its wiki dev page URL.
/// Used for resolving snippet citation IDs (e.g. `[C1-S2]`) to real links.
fn build_file_dev_map(
    nodes: &[Node],
    feature_of: &dyn Fn(&str, &str) -> String,
) -> HashMap<String, String> {
    use std::collections::BTreeSet;

    // Group class-like nodes by feature; track id→name and id→file for lookup.
    let mut by_feature: std::collections::BTreeMap<String, BTreeSet<String>> =
        std::collections::BTreeMap::new();
    let mut id_to_name: HashMap<String, String> = HashMap::new();
    let mut id_to_file: HashMap<String, String> = HashMap::new();

    for node in nodes {
        if !matches!(
            node.kind,
            cih_core::NodeKind::Class
                | cih_core::NodeKind::Interface
                | cih_core::NodeKind::Enum
                | cih_core::NodeKind::Record
        ) || node.file.is_empty()
        {
            continue;
        }
        let id = node.id.as_str().to_string();
        let feature = feature_of(id.as_str(), node.file.as_str());
        by_feature.entry(feature).or_default().insert(id.clone());
        id_to_name
            .entry(id.clone())
            .or_insert_with(|| node.name.clone());
        id_to_file.entry(id).or_insert_with(|| node.file.clone());
    }

    // For each feature, use the same collision-aware slug algorithm as lib.rs,
    // then map each class's source file to its canonical dev-page URL.
    let mut file_to_url: HashMap<String, String> = HashMap::new();
    for (feature, class_ids) in by_feature {
        let slugs = assign_class_slugs(&class_ids, |id| {
            id_to_name.get(id).cloned().unwrap_or_else(|| {
                id.trim_start_matches("Class:")
                    .rsplit('.')
                    .next()
                    .unwrap_or("Unknown")
                    .to_string()
            })
        });
        for (class_id, slug) in slugs {
            if let Some(file) = id_to_file.get(&class_id) {
                let url = format!("/docs/{}/dev/{}", feature, slug);
                // Keep first match per file (avoids inner classes overwriting the outer one).
                file_to_url.entry(file.clone()).or_insert(url);
            }
        }
    }
    file_to_url
}

/// Builds a citation-ID → dev-page-URL map for one feature's merged community evidence.
/// Mirrors the `[C{n}-S{m}]` numbering used in `build_feature_evidence`.
fn build_feature_citation_map(
    community_ids: &[String],
    graph: &WikiGraph,
    repo: &Path,
    corpus: &EvidenceCorpus,
    file_dev_map: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut citation_map: HashMap<String, String> = HashMap::new();
    let mut seen_texts: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for (community_idx, comm_id) in community_ids.iter().enumerate() {
        let Some(comm_node) = graph.nodes_by_id.get(comm_id) else {
            continue;
        };
        let pack = build_evidence_pack(Some(repo), graph, comm_node, corpus);
        let community_evidence_id = format!("C{}", community_idx + 1);

        for item in &pack.items {
            if seen_texts.contains(&item.text) {
                continue;
            }
            seen_texts.insert(item.text.clone());

            if let Some(file) = item.snippet_file() {
                if let Some(url) = file_dev_map.get(file) {
                    let citation_id = format!("{}-{}", community_evidence_id, item.id);
                    citation_map.insert(citation_id, url.clone());
                }
            }
        }
    }
    citation_map
}

/// Replaces bare citation IDs (e.g. `[C1-S2]`) with Markdown links when a URL is known.
/// Leaves citations unchanged when no mapping is available.
fn replace_citations(text: &str, map: &HashMap<String, String>) -> String {
    if map.is_empty() || !text.contains('[') {
        return text.to_string();
    }
    let mut out = text.to_string();
    for (citation_id, url) in map {
        let bare = format!("[{}]", citation_id);
        // Only replace if not already a link (not followed by '(')
        let linked = format!("[{}]({})", citation_id, url);
        // Avoid double-linking: skip if bare pattern is already followed by '('
        let mut pos = 0;
        while let Some(idx) = out[pos..].find(&bare) {
            let abs = pos + idx;
            let after = abs + bare.len();
            let next = out.as_bytes().get(after).copied();
            if next == Some(b'(') {
                // Already a link — skip
                pos = after;
            } else if next.map(|c| c.is_ascii_alphanumeric()).unwrap_or(false) {
                // Substring of a longer ID, e.g. [C1-S2] inside [C1-S20] — skip
                pos = after;
            } else {
                out.replace_range(abs..after, &linked);
                pos = abs + linked.len();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmRequest, LlmResponse};
    use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};
    use std::sync::{
        atomic::{AtomicUsize, Ordering as AOrdering},
        Mutex,
    };
    use std::collections::VecDeque;

    struct MockLlm {
        responses: Mutex<VecDeque<Result<String>>>,
        pub calls: AtomicUsize,
    }
    impl MockLlm {
        fn new(responses: Vec<Result<String>>) -> Self {
            Self { responses: Mutex::new(responses.into()), calls: AtomicUsize::new(0) }
        }
    }
    impl LlmAdapter for MockLlm {
        fn call(&self, _key: Option<&str>, _req: &LlmRequest) -> Result<LlmResponse> {
            self.calls.fetch_add(1, AOrdering::SeqCst);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no more mock responses")))
                .map(|text| LlmResponse { text })
        }
    }

    fn node(id: &str, kind: NodeKind, name: &str) -> Node {
        Node {
            id: NodeId::new(id.to_string()),
            kind,
            name: name.to_string(),
            qualified_name: None,
            file: "com/example/modules/orders/OrderController.java".to_string(),
            range: Range::default(),
            props: None,
        }
    }

    fn edge(src: &str, dst: &str, kind: EdgeKind) -> Edge {
        Edge {
            src: NodeId::new(src.to_string()),
            dst: NodeId::new(dst.to_string()),
            kind,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        }
    }

    fn flow_json(narrative: &str) -> String {
        format!(
            r#"{{"narrative": "{narrative}", "business_impact": "Important.", "step_descriptions": ["Queries the service"]}}"#
        )
    }

    #[test]
    fn flow_cache_hit_skips_llm_on_second_call() {
        let handler_id = "Method:com.example.modules.orders.OrderController#list/0";
        let ctrl_cls = "Class:com.example.modules.orders.OrderController";
        let service_id = "Method:com.example.modules.orders.OrderService#findAll/0";
        let svc_cls = "Class:com.example.modules.orders.OrderService";
        let route_id = "Route:GET:/orders";

        let nodes = vec![
            node(ctrl_cls, NodeKind::Class, "OrderController"),
            node(handler_id, NodeKind::Method, "list"),
            node(svc_cls, NodeKind::Class, "OrderService"),
            node(service_id, NodeKind::Method, "findAll"),
            node(route_id, NodeKind::Route, "GET /orders"),
        ];
        let edges = vec![
            edge(ctrl_cls, handler_id, EdgeKind::HasMethod),
            edge(svc_cls, service_id, EdgeKind::HasMethod),
            edge(handler_id, route_id, EdgeKind::HandlesRoute),
            edge(handler_id, service_id, EdgeKind::Calls),
        ];
        let graph = WikiGraph::build(&nodes, &edges, &[], &[]);

        let flow_response = flow_json("Lists all orders for the customer.");

        // First run: LLM should be called once.
        let adapter1 = MockLlm::new(vec![Ok(flow_response.clone())]);
        let empty_cache = BTreeMap::new();
        let (summaries1, updates1) = enrich_route_flows(
            &graph, None, &adapter1, None, "model", 1000, 30, 0, "en", false,
            &empty_cache, 1,
        );
        assert_eq!(adapter1.calls.load(AOrdering::SeqCst), 1, "first run must call LLM");
        assert!(summaries1.contains_key(handler_id));
        assert_eq!(updates1.len(), 1);

        // Build flow_cache from the first run's updates.
        let mut flow_cache: BTreeMap<String, FlowCacheEntry> = BTreeMap::new();
        for (id, ev_hash, summary) in updates1 {
            flow_cache.insert(id, FlowCacheEntry { evidence_hash: ev_hash, summary });
        }

        // Second run: same graph + populated cache → cache hit, LLM must NOT be called.
        let adapter2 = MockLlm::new(vec![]); // empty — any call panics via "no more mock responses"
        let (summaries2, updates2) = enrich_route_flows(
            &graph, None, &adapter2, None, "model", 1000, 30, 0, "en", false,
            &flow_cache, 1,
        );
        assert_eq!(adapter2.calls.load(AOrdering::SeqCst), 0, "second run must hit cache");
        assert!(summaries2.contains_key(handler_id));
        assert_eq!(summaries2[handler_id].narrative, summaries1[handler_id].narrative);
        assert!(updates2.is_empty(), "cache hit must not produce new updates");
    }

    #[test]
    fn flow_cache_miss_on_changed_call_chain() {
        let handler_id = "Method:com.example.modules.orders.OrderController#list/0";
        let ctrl_cls = "Class:com.example.modules.orders.OrderController";
        let service_id = "Method:com.example.modules.orders.OrderService#findAll/0";
        let svc_cls = "Class:com.example.modules.orders.OrderService";
        let extra_id = "Method:com.example.modules.orders.OrderRepo#count/0";
        let repo_cls = "Class:com.example.modules.orders.OrderRepo";
        let route_id = "Route:GET:/orders";

        // Graph v1: handler → service
        let nodes_v1 = vec![
            node(ctrl_cls, NodeKind::Class, "OrderController"),
            node(handler_id, NodeKind::Method, "list"),
            node(svc_cls, NodeKind::Class, "OrderService"),
            node(service_id, NodeKind::Method, "findAll"),
            node(route_id, NodeKind::Route, "GET /orders"),
        ];
        let edges_v1 = vec![
            edge(ctrl_cls, handler_id, EdgeKind::HasMethod),
            edge(svc_cls, service_id, EdgeKind::HasMethod),
            edge(handler_id, route_id, EdgeKind::HandlesRoute),
            edge(handler_id, service_id, EdgeKind::Calls),
        ];
        let graph_v1 = WikiGraph::build(&nodes_v1, &edges_v1, &[], &[]);

        let adapter1 = MockLlm::new(vec![Ok(flow_json("Lists orders."))]);
        let empty_cache = BTreeMap::new();
        let (_, updates1) = enrich_route_flows(
            &graph_v1, None, &adapter1, None, "model", 1000, 30, 0, "en", false,
            &empty_cache, 1,
        );
        let mut flow_cache: BTreeMap<String, FlowCacheEntry> = BTreeMap::new();
        for (id, ev_hash, summary) in updates1 {
            flow_cache.insert(id, FlowCacheEntry { evidence_hash: ev_hash, summary });
        }

        // Graph v2: handler → service → extra (deeper chain)
        let nodes_v2 = vec![
            node(ctrl_cls, NodeKind::Class, "OrderController"),
            node(handler_id, NodeKind::Method, "list"),
            node(svc_cls, NodeKind::Class, "OrderService"),
            node(service_id, NodeKind::Method, "findAll"),
            node(repo_cls, NodeKind::Class, "OrderRepo"),
            node(extra_id, NodeKind::Method, "count"),
            node(route_id, NodeKind::Route, "GET /orders"),
        ];
        let edges_v2 = vec![
            edge(ctrl_cls, handler_id, EdgeKind::HasMethod),
            edge(svc_cls, service_id, EdgeKind::HasMethod),
            edge(repo_cls, extra_id, EdgeKind::HasMethod),
            edge(handler_id, route_id, EdgeKind::HandlesRoute),
            edge(handler_id, service_id, EdgeKind::Calls),
            edge(service_id, extra_id, EdgeKind::Calls),
        ];
        let graph_v2 = WikiGraph::build(&nodes_v2, &edges_v2, &[], &[]);

        // Cache from v1 should not match v2's chain; LLM called once.
        let adapter2 = MockLlm::new(vec![Ok(flow_json("Lists orders with count."))]);
        let (summaries2, _) = enrich_route_flows(
            &graph_v2, None, &adapter2, None, "model", 1000, 30, 0, "en", false,
            &flow_cache, 1,
        );
        assert_eq!(adapter2.calls.load(AOrdering::SeqCst), 1, "cache miss must call LLM");
        assert!(summaries2.contains_key(handler_id));
    }
}
