use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};
use cih_core::{ArchitectureHint, EdgeKind, GraphArtifacts, NodeKind, RepoMap, VersionId};
use cih_grouping::{
    apply_overrides, feature_artifact_dir, find_feature_artifact_dir, prune_feature_artifacts,
    read_feature_artifact, write_feature_artifacts, FeatureOverrides, FeatureStrategy,
    PackageConfig, StrategyInput,
};

use crate::feature_strategy::{build_feature_strategy, FeatureLlmOptions};
use crate::llm::LlmCallConfig;

/// Feature-classification strategy for community grouping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FeatureStrategyKind {
    #[default]
    Package,
    Structural,
    Hybrid,
    Llm,
    /// Primary embedding clusterer (k-NN + Leiden over pgvector). Requires a prior `cih embed`.
    Embed,
}

impl std::fmt::Display for FeatureStrategyKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Package => "package",
            Self::Structural => "structural",
            Self::Hybrid => "hybrid",
            Self::Llm => "llm",
            Self::Embed => "embed",
        })
    }
}

impl std::str::FromStr for FeatureStrategyKind {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "" | "package" => Ok(Self::Package),
            "structural" => Ok(Self::Structural),
            "hybrid" => Ok(Self::Hybrid),
            "llm" => Ok(Self::Llm),
            "embed" => Ok(Self::Embed),
            other => anyhow::bail!(
                "unknown --feature-strategy '{}'; expected package | structural | hybrid | llm | embed",
                other
            ),
        }
    }
}
use serde::Serialize;

use crate::db::{load_many_to_falkor, LoadOutcome};
use crate::versioning::{discover_version, latest_graph_artifacts, prune_other_versions};
use crate::{DEFAULT_FALKOR_URL, DEFAULT_GRAPH_KEY};

/// CLI overrides for community detection, process tracing, and feature grouping.
#[derive(Default)]
pub struct DiscoverOverrides {
    /// Community detection strategy: "package" (default) or "graph" (Leiden).
    pub community_strategy: String,
    // Leiden-only overrides (ignored when community_strategy is "package").
    pub resolution: Option<f64>,
    pub min_community_size: Option<usize>,
    pub max_trace_depth: Option<usize>,
    pub max_processes: Option<usize>,
    pub max_branching: Option<usize>,
    pub min_trace_confidence: Option<f32>,
    /// Feature classification strategy.
    pub feature_strategy: FeatureStrategyKind,
    /// LLM config — required when feature_strategy is "llm" or "hybrid".
    pub feature_llm: Option<LlmCallConfig>,
    /// Postgres URL for `--feature-strategy embed` (falls back to $CIH_PG_URL upstream).
    pub pg_url: Option<String>,
    /// Embed clusterer knobs (None → EmbedClusterConfig defaults).
    pub embed_similarity_threshold: Option<f32>,
    pub embed_knn: Option<usize>,
    pub embed_leiden_resolution: Option<f64>,
}

pub fn run_discover(
    repo: PathBuf,
    falkor_url: Option<String>,
    graph_key: Option<String>,
    no_load: bool,
    json: bool,
    overrides: DiscoverOverrides,
) -> Result<()> {
    let span = tracing::info_span!("discover", repo = %repo.display());
    let _enter = span.enter();

    tracing::info!(repo = %repo.display(), "starting discover");

    let emit = run_discover_core(&repo, &overrides)?;

    let load = if no_load {
        tracing::info!("Skipping FalkorDB load (--no-load)");
        LoadOutcome::Skipped
    } else {
        let url = falkor_url.as_deref().unwrap_or(DEFAULT_FALKOR_URL);
        let key = graph_key.as_deref().unwrap_or(DEFAULT_GRAPH_KEY);
        let artifact_sets = emit.artifact_sets_for_load();
        match load_many_to_falkor(url, key, &artifact_sets) {
            Ok(stats) => {
                tracing::info!(
                    nodes = stats.nodes,
                    edges = stats.edges,
                    "FalkorDB discover load complete"
                );
                LoadOutcome::Loaded(stats)
            }
            Err(err) => {
                tracing::warn!(error = %err, "FalkorDB discover load failed");
                LoadOutcome::Failed(format!("{err:#}"))
            }
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&emit.summary(&load))?);
    } else {
        emit.print_styled(&load);
    }

    crate::registry::persist_discover(&repo, &emit);

    if matches!(load, LoadOutcome::Failed(_)) {
        process::exit(3);
    }
    Ok(())
}

pub fn run_discover_core(repo: &Path, overrides: &DiscoverOverrides) -> Result<DiscoverOutcome> {
    let mut ui = crate::ui::PhaseProgress::new();
    ui.spin("Loading graph");

    let source = latest_graph_artifacts(repo)?;
    let nodes = source
        .read_nodes()
        .with_context(|| format!("failed to read {}", source.nodes_path.display()))?;
    let edges = source
        .read_edges()
        .with_context(|| format!("failed to read {}", source.edges_path.display()))?;

    tracing::info!(
        source_version = %source.version.0,
        nodes = nodes.len(),
        edges = edges.len(),
        "source graph loaded"
    );
    ui.finish_with(format!(
        "{} nodes, {} edges",
        crate::ui::fmt_count(nodes.len()),
        crate::ui::fmt_count(edges.len())
    ));

    // Load architecture hint from repo-map.json (written during scan/analyze).
    let arch_hint = read_architecture_hint(repo);
    tracing::info!(
        architecture_hint = ?arch_hint,
        "architecture hint loaded"
    );

    let use_leiden = overrides.community_strategy == "graph";

    ui.spin("Detecting communities");
    let community_output = if use_leiden {
        let mut community_cfg = cih_community::CommunityConfig::default();
        if cih_community::is_large_graph(&nodes) {
            tracing::info!(
                nodes = nodes.len(),
                resolution = 1.0,
                max_iterations = 3,
                min_community_size = 3,
                "large graph detected — using conservative Leiden resolution"
            );
            community_cfg.max_iterations = 3;
            community_cfg.min_community_size = 3;
        }
        if arch_hint == ArchitectureHint::Monolith && community_cfg.min_community_size < 5 {
            tracing::info!(
                min_community_size = 5,
                "monolith detected — raising min_community_size to reduce over-fragmentation"
            );
            community_cfg.min_community_size = 5;
        }
        if let Some(v) = overrides.resolution {
            community_cfg.resolution = v;
        }
        if let Some(v) = overrides.min_community_size {
            community_cfg.min_community_size = v;
        }
        tracing::info!(
            resolution = community_cfg.resolution,
            min_community_size = community_cfg.min_community_size,
            "running Leiden community detection"
        );
        cih_community::detect_communities(&nodes, &edges, &community_cfg)
    } else {
        let pkg_cfg = PackageConfig::load_or_default(repo);
        let pkg_strategy = cih_grouping::PackageStrategy::new(pkg_cfg);
        tracing::info!("running package-based community detection");
        cih_community::detect_communities_from_packages(&nodes, &edges, &|file| {
            pkg_strategy.feature_of(file)
        })
    };
    tracing::info!(
        communities = community_output.nodes.len(),
        edges = community_output.edges.len(),
        strategy = if use_leiden { "graph" } else { "package" },
        "community detection complete"
    );
    ui.finish_with(format!(
        "{} communities",
        crate::ui::fmt_count(community_output.nodes.len())
    ));

    let symbol_count = nodes
        .iter()
        .filter(|n| {
            matches!(
                n.kind,
                NodeKind::Method | NodeKind::Constructor | NodeKind::Class | NodeKind::Interface
            )
        })
        .count();
    tracing::debug!(symbols = symbol_count, "symbol count for process config");
    let mut process_cfg = cih_community::ProcessConfig::for_symbol_count(symbol_count);
    // Apply CLI overrides on top of heuristics.
    if let Some(v) = overrides.max_trace_depth {
        process_cfg.max_trace_depth = v;
    }
    if let Some(v) = overrides.max_processes {
        process_cfg.max_processes = v;
    }
    if let Some(v) = overrides.max_branching {
        process_cfg.max_branching = v;
    }
    if let Some(v) = overrides.min_trace_confidence {
        process_cfg.min_trace_confidence = v;
    }

    let entry_registry = cih_core::EntrypointRegistry::load(repo);
    tracing::info!(
        patterns = entry_registry.total_patterns(),
        "entry-point registry loaded"
    );

    tracing::info!("tracing business processes");
    ui.spin("Tracing processes");
    let process_output = cih_community::trace_processes(
        &nodes,
        &edges,
        &community_output.memberships,
        &process_cfg,
        &entry_registry,
    );
    tracing::info!(
        processes = process_output.nodes.len(),
        edges = process_output.edges.len(),
        "process tracing complete"
    );
    ui.finish_with(format!(
        "{} processes",
        crate::ui::fmt_count(process_output.nodes.len())
    ));

    // Write entrypoints sidecar — persists Scheduled/EventListener methods that are
    // detected in-memory but not stored in any graph artifact.
    write_entrypoints_sidecar(
        repo,
        &nodes,
        &edges,
        process_cfg.min_trace_confidence,
        &entry_registry,
    );

    let mut output_nodes = community_output.nodes;
    output_nodes.extend(process_output.nodes);
    let mut output_edges = community_output.edges;
    output_edges.extend(process_output.edges);
    output_nodes.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    output_edges.sort_by(|a, b| {
        a.src
            .as_str()
            .cmp(b.src.as_str())
            .then_with(|| a.dst.as_str().cmp(b.dst.as_str()))
            .then_with(|| a.kind.cypher_label().cmp(b.kind.cypher_label()))
    });

    let version = discover_version(&output_nodes, &output_edges);
    tracing::debug!(version = %version, "community version computed");

    ui.spin("Writing artifacts");
    let artifacts_dir = repo.join(".cih").join("artifacts-community").join(&version);
    let artifacts = GraphArtifacts::write(
        &artifacts_dir,
        VersionId(version.clone()),
        &output_nodes,
        &output_edges,
    )
    .with_context(|| {
        format!(
            "failed to write community artifacts to {}",
            artifacts_dir.display()
        )
    })?;
    prune_other_versions(&repo.join(".cih").join("artifacts-community"), &version)?;

    tracing::info!(
        version = %version,
        path = %artifacts_dir.display(),
        nodes = output_nodes.len(),
        edges = output_edges.len(),
        "community artifacts written"
    );
    ui.finish_with(format!(
        "{} nodes, {} edges  \x1b[2m(v{})\x1b[0m",
        crate::ui::fmt_count(output_nodes.len()),
        crate::ui::fmt_count(output_edges.len()),
        &version[..8.min(version.len())]
    ));

    // ── Feature artifacts ─────────────────────────────────────────────────────
    let feature_strategy_kind = overrides.feature_strategy;
    ui.spin(format!("Grouping features ({})", feature_strategy_kind));
    let pkg_cfg = PackageConfig::load_or_default(repo);

    // Build optional LLM caller from CLI config.
    let llm_opts: Option<FeatureLlmOptions> = if let Some(llm_cfg) = &overrides.feature_llm {
        match crate::llm::make_adapter(&llm_cfg.provider, &llm_cfg.base_url, None) {
            Ok(adapter) => {
                // Load prior artifact for incremental cache.
                let prior_artifact = find_feature_artifact_dir(repo, &source.version.0)
                    .and_then(|dir| read_feature_artifact(&dir).ok())
                    .unwrap_or_default();
                let prior_artifact = prior_artifact
                    .iter()
                    .filter(|e| e.strategy == "llm")
                    .cloned()
                    .collect();
                let api_key = crate::llm::resolve_api_key(llm_cfg.api_key_env.as_deref())
                    .ok()
                    .flatten();
                Some(FeatureLlmOptions {
                    adapter,
                    api_key,
                    model: llm_cfg.model.clone(),
                    max_tokens: llm_cfg.max_tokens,
                    timeout_secs: llm_cfg.timeout_secs,
                    prior_artifact,
                })
            }
            Err(err) => {
                tracing::warn!(error = %err, "feature LLM adapter failed to build — LLM stage disabled");
                None
            }
        }
    } else {
        None
    };

    let feature_strategy = if feature_strategy_kind == FeatureStrategyKind::Embed {
        // Embedding clusterer: owns the Postgres + Leiden work, then hands assignments to the
        // (Postgres-free) grouping strategy. Any failure degrades cleanly to package.
        match build_embed_cluster_strategy(&nodes, overrides, repo, &source.version.0) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(
                    error = %format!("{err:#}"),
                    "embed feature strategy unavailable — falling back to package \
                     (did you run `cih embed --pg-url` first?)"
                );
                Box::new(cih_grouping::PackageStrategy::new(
                    PackageConfig::load_or_default(repo),
                ))
            }
        }
    } else {
        match build_feature_strategy(feature_strategy_kind, pkg_cfg, llm_opts) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(
                    strategy = %feature_strategy_kind,
                    error = %err,
                    "feature strategy failed to load — falling back to package"
                );
                Box::new(cih_grouping::PackageStrategy::new(
                    PackageConfig::load_or_default(repo),
                ))
            }
        }
    };
    let strategy_input = StrategyInput {
        nodes: &nodes,
        edges: &edges,
        graph_version: &source.version.0,
        prior_assignments: &[],
    };
    let raw_entries = feature_strategy.assign(&strategy_input);
    let merged_entries = match FeatureOverrides::load(repo) {
        Some(ov) if !ov.is_empty() => {
            tracing::info!(overrides = ov.len(), "applying feature overrides");
            apply_overrides(raw_entries.clone(), &ov)
        }
        _ => raw_entries.clone(),
    };
    let feature_count = {
        let mut names = std::collections::HashSet::new();
        for e in &merged_entries {
            names.insert(e.name.as_str());
        }
        names.len()
    };
    let feat_dir = feature_artifact_dir(repo, &source.version.0);
    write_feature_artifacts(
        &feat_dir,
        feature_strategy.name(),
        &raw_entries,
        &merged_entries,
    )
    .with_context(|| {
        format!(
            "failed to write feature artifacts to {}",
            feat_dir.display()
        )
    })?;
    prune_feature_artifacts(
        &repo.join(".cih").join("artifacts-features"),
        &source.version.0,
    )?;
    tracing::info!(
        features = feature_count,
        entries = merged_entries.len(),
        "feature artifacts written"
    );
    ui.finish_with(format!("{} features", crate::ui::fmt_count(feature_count)));

    let route_count = nodes.iter().filter(|n| n.kind == NodeKind::Route).count();

    Ok(DiscoverOutcome {
        source_artifacts: source,
        artifacts,
        artifacts_dir,
        version,
        route_count,
        community_count: output_nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Community)
            .count(),
        process_count: output_nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Process)
            .count(),
        member_edge_count: output_edges
            .iter()
            .filter(|e| e.kind == EdgeKind::MemberOf)
            .count(),
        step_edge_count: output_edges
            .iter()
            .filter(|e| e.kind == EdgeKind::StepInProcess)
            .count(),
        node_count: output_nodes.len(),
        edge_count: output_edges.len(),
        feature_count,
    })
}

/// A node eligible for embed feature clustering: first-party (not from a jar / not `external`) and
/// not a test source. Third-party and test nodes are excluded so they don't form junk features.
fn is_project_node(n: &cih_core::Node) -> bool {
    let is_external = n
        .props
        .as_ref()
        .map(|p| {
            p.get("external").and_then(|v| v.as_bool()).unwrap_or(false)
                || p.get("fromJar").and_then(|v| v.as_bool()).unwrap_or(false)
        })
        .unwrap_or(false);
    let f = n.file.as_str();
    let is_test = f.ends_with(".jar")
        || f.contains("src/test/")
        || f.contains("/test/java/")
        || f.contains("/test/kotlin/");
    !is_external && !is_test
}

/// Build the embedding clusterer: connect to pgvector, fetch per-node vectors + k-NN edges, run
/// weighted Leiden, and hand the precomputed assignments to the Postgres-free
/// `EmbedClusterStrategy`. All async DB work runs on `crate::runtime::block_on` — the codebase's
/// single shared Tokio runtime — which sidesteps the nested-runtime panics a fresh
/// `Runtime::new().block_on()` would cause if discover were ever driven from within a runtime.
fn build_embed_cluster_strategy(
    nodes: &[cih_core::Node],
    overrides: &DiscoverOverrides,
    repo: &Path,
    source_version: &str,
) -> Result<Box<dyn FeatureStrategy>> {
    use std::collections::{HashMap, HashSet};

    let pg_url = overrides
        .pg_url
        .clone()
        .or_else(|| std::env::var("CIH_PG_URL").ok())
        .context(
            "--feature-strategy embed requires Postgres: pass --pg-url or set CIH_PG_URL, \
             and run `cih embed` first",
        )?;

    let mut cfg = cih_grouping::EmbedClusterConfig::default();
    if let Some(v) = overrides.embed_similarity_threshold {
        cfg.similarity_threshold = v;
    }
    if let Some(v) = overrides.embed_knn {
        cfg.knn = v;
    }
    if let Some(v) = overrides.embed_leiden_resolution {
        cfg.leiden_resolution = v;
    }

    // Full embeddable set (used for the cih_node_vectors backfill/prune authority — it must match
    // what `cih embed` maintains, so semantic search keeps working).
    let node_ids: Vec<String> = cih_embed::embeddable_nodes(nodes)
        .iter()
        .map(|n| n.id.as_str().to_string())
        .collect();
    if node_ids.is_empty() {
        anyhow::bail!("no embeddable nodes in the current graph");
    }

    // Clustering universe: project, non-test nodes only. Third-party types (jsoup/AWS SDK/...) are
    // flagged `props.external`/`fromJar` and otherwise cluster into junk features; test sources
    // pollute domain features. We keep the k-NN computed over the full table (recall) but restrict
    // clustering to this subset by filtering edges below.
    let project_ids: std::collections::HashSet<String> = cih_embed::embeddable_nodes(nodes)
        .iter()
        .filter(|n| is_project_node(n))
        .map(|n| n.id.as_str().to_string())
        .collect();
    if project_ids.is_empty() {
        anyhow::bail!("no project (non-external, non-test) embeddable nodes to cluster");
    }
    let project_vec: Vec<String> = project_ids.iter().cloned().collect();

    let knn = cfg.knn;
    let thr = cfg.similarity_threshold;
    let (vectors_raw, mut edges) = crate::runtime::block_on(async {
        let store =
            cih_embed::EmbedStore::connect(&pg_url, cih_embed::EmbedModelKind::MiniLm).await?;
        store.ensure_schema().await?;
        // Self-heal: an older `cih embed` may predate cih_node_vectors — backfill if empty.
        if store.node_vector_count().await? == 0 {
            tracing::info!("cih_node_vectors empty — backfilling from cih_embeddings");
            store.upsert_node_vectors(&node_ids).await?;
            store.prune_node_vectors(&node_ids).await?;
        }
        // Only fetch vectors/meta for the project subset we actually cluster.
        let vectors = store.node_vectors(&project_vec).await?;
        let edges = store.knn_edges(knn, thr).await?;
        anyhow::Ok((vectors, edges))
    })?;

    if vectors_raw.is_empty() {
        anyhow::bail!("no embeddings found for current project nodes — run `cih embed` first");
    }

    // Keep only edges whose endpoints are both in the project subset. External types are
    // semantically self-similar, so dropping their edges cleanly removes junk clusters without
    // starving project clusters.
    let edges_before = edges.len();
    edges.retain(|(s, d, _)| {
        project_ids.contains(s.as_str()) && project_ids.contains(d.as_str())
    });
    tracing::info!(
        project = project_ids.len(),
        embeddable = node_ids.len(),
        vectors = vectors_raw.len(),
        edges = edges.len(),
        edges_dropped = edges_before - edges.len(),
        "embed clustering: fetched per-node vectors and k-NN edges (project subset)"
    );
    if edges.is_empty() {
        tracing::warn!(
            threshold = thr,
            "no k-NN edges above similarity threshold — lower --embed-similarity-threshold; \
             all nodes will be unclustered (shared)"
        );
    }

    // Weighted Leiden over the semantic k-NN graph (petgraph stays inside cih-community).
    let clusters =
        cih_community::cluster_similarity_edges(&edges, cfg.leiden_resolution, cfg.leiden_seed, 10);
    let cluster_count = clusters.iter().map(|(_, c)| *c).collect::<HashSet<_>>().len();
    tracing::info!(
        clusters = cluster_count,
        assigned = clusters.len(),
        "embed clustering: Leiden produced clusters"
    );

    // Split the fetched rows into the (vectors, meta) maps the strategy needs.
    let mut vectors: HashMap<String, Vec<f32>> = HashMap::with_capacity(vectors_raw.len());
    let mut meta: HashMap<String, cih_grouping::NodeMeta> =
        HashMap::with_capacity(vectors_raw.len());
    for nv in vectors_raw {
        let id = nv.node_id.as_str().to_string();
        meta.insert(
            id.clone(),
            cih_grouping::NodeMeta { kind: nv.node_kind, name: nv.name, file: nv.file },
        );
        vectors.insert(id, nv.vector);
    }
    let clusters: Vec<(String, usize)> = clusters
        .into_iter()
        .map(|(id, c)| (id.as_str().to_string(), c))
        .collect();

    // Opt-in LLM labeling: build a caller only when a feature-LLM provider is configured. Also load
    // the prior run's embed entries so unchanged clusters reuse their prior LLM name (stability).
    let (llm_caller, prior_entries) = match &overrides.feature_llm {
        Some(llm_cfg) => match crate::llm::make_adapter(&llm_cfg.provider, &llm_cfg.base_url, None) {
            Ok(adapter) => {
                let api_key = crate::llm::resolve_api_key(llm_cfg.api_key_env.as_deref())
                    .ok()
                    .flatten();
                let caller = crate::feature_strategy::make_feature_llm_caller(
                    adapter,
                    api_key,
                    llm_cfg.model.clone(),
                    llm_cfg.max_tokens,
                    llm_cfg.timeout_secs,
                );
                let prior = find_feature_artifact_dir(repo, source_version)
                    .and_then(|dir| read_feature_artifact(&dir).ok())
                    .unwrap_or_default();
                (Some(caller), prior)
            }
            Err(err) => {
                tracing::warn!(error = %err, "embed LLM labeling disabled — adapter failed to build");
                (None, Vec::new())
            }
        },
        None => (None, Vec::new()),
    };

    Ok(Box::new(cih_grouping::EmbedClusterStrategy::new(
        clusters,
        vectors,
        meta,
        cfg,
        llm_caller,
        prior_entries,
    )))
}

/// Everything `run_discover_core` produced (DB-free), used to load + report.
pub struct DiscoverOutcome {
    pub source_artifacts: GraphArtifacts,
    pub artifacts: GraphArtifacts,
    pub artifacts_dir: PathBuf,
    pub version: String,
    pub route_count: usize,
    pub community_count: usize,
    pub process_count: usize,
    pub member_edge_count: usize,
    pub step_edge_count: usize,
    pub node_count: usize,
    pub edge_count: usize,
    pub feature_count: usize,
}

impl DiscoverOutcome {
    pub fn artifact_sets_for_load(&self) -> [&GraphArtifacts; 2] {
        [&self.source_artifacts, &self.artifacts]
    }

    fn summary<'a>(&'a self, load: &'a LoadOutcome) -> DiscoverSummary<'a> {
        DiscoverSummary {
            source_version: self.source_artifacts.version.0.as_str(),
            version: &self.version,
            artifacts_path: self.artifacts_dir.display().to_string(),
            community_count: self.community_count,
            process_count: self.process_count,
            feature_count: self.feature_count,
            member_edge_count: self.member_edge_count,
            step_edge_count: self.step_edge_count,
            node_count: self.node_count,
            edge_count: self.edge_count,
            falkor_status: load.status(),
            falkor_nodes: load.stats().map(|s| s.nodes),
            falkor_edges: load.stats().map(|s| s.edges),
            falkor_error: load.error(),
        }
    }

    fn print_styled(&self, load: &LoadOutcome) {
        let ver = &self.version[..8.min(self.version.len())];
        crate::ui::print_header("Discover", "", Some(ver));
        crate::ui::print_row("Communities", &crate::ui::fmt_count(self.community_count));
        crate::ui::print_row("Processes", &crate::ui::fmt_count(self.process_count));
        crate::ui::print_row("Features", &crate::ui::fmt_count(self.feature_count));
        crate::ui::print_row(
            "Edges",
            &format!(
                "{}  member  {}  process",
                crate::ui::fmt_count(self.member_edge_count),
                crate::ui::fmt_count(self.step_edge_count)
            ),
        );
        crate::ui::print_row("Artifacts", &self.artifacts_dir.display().to_string());
        let falkor_str = match load {
            LoadOutcome::Loaded(stats) => format!(
                "{}  nodes  {}  edges",
                crate::ui::fmt_count(stats.nodes as usize),
                crate::ui::fmt_count(stats.edges as usize)
            ),
            LoadOutcome::Skipped => "\x1b[2mskipped (--no-load)\x1b[0m".to_string(),
            LoadOutcome::Reused => "\x1b[2mreused (no changes)\x1b[0m".to_string(),
            LoadOutcome::Failed(e) => format!("\x1b[31mfailed\x1b[0m  \x1b[2m{e}\x1b[0m"),
        };
        crate::ui::print_row("FalkorDB", &falkor_str);
        eprintln!();
    }
}


#[derive(Serialize)]
struct DiscoverSummary<'a> {
    source_version: &'a str,
    version: &'a str,
    artifacts_path: String,
    community_count: usize,
    process_count: usize,
    feature_count: usize,
    member_edge_count: usize,
    step_edge_count: usize,
    node_count: usize,
    edge_count: usize,
    /// "loaded" | "skipped" | "failed"
    falkor_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    falkor_nodes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    falkor_edges: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    falkor_error: Option<&'a str>,
}

/// Record written per method into `.cih/entrypoints.json`.
#[derive(Serialize)]
struct EntrypointRecord {
    method_id: String,
    kind: String,
    /// Topic names for EventListener; empty for Scheduled.
    topics: Vec<String>,
}

fn write_entrypoints_sidecar(
    repo: &Path,
    nodes: &[cih_core::Node],
    edges: &[cih_core::Edge],
    min_confidence: f32,
    registry: &cih_core::EntrypointRegistry,
) {
    let scored = cih_core::score_all_entry_points(nodes, edges, min_confidence, registry);
    let records: Vec<EntrypointRecord> = scored
        .into_iter()
        .filter(|ep| {
            matches!(
                ep.kind,
                cih_core::EntrypointKind::Scheduled | cih_core::EntrypointKind::EventListener
            )
        })
        .map(|ep| EntrypointRecord {
            method_id: ep.id.as_str().to_string(),
            kind: ep.kind.as_str().to_string(),
            topics: ep.event_topics,
        })
        .collect();

    if records.is_empty() {
        return;
    }

    let path = repo.join(".cih").join("entrypoints.json");
    match serde_json::to_string_pretty(&records) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!(error = %e, "failed to write entrypoints sidecar");
            } else {
                tracing::info!(count = records.len(), path = %path.display(), "entrypoints sidecar written");
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to serialize entrypoints"),
    }
}

fn read_architecture_hint(repo: &Path) -> ArchitectureHint {
    let path = repo.join(".cih").join("repo-map.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return ArchitectureHint::Unknown;
    };
    serde_json::from_str::<RepoMap>(&raw)
        .map(|rm| rm.architecture_hint)
        .unwrap_or(ArchitectureHint::Unknown)
}
