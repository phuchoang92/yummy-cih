use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};
use cih_core::{ArchitectureHint, EdgeKind, GraphArtifacts, NodeKind, RepoMap, VersionId};
use cih_grouping::{
    apply_overrides, feature_artifact_dir, prune_feature_artifacts, write_feature_artifacts,
    FeatureOverrides, FeatureStrategy, PackageConfig, StrategyInput,
};

use crate::grouping::build_feature_strategy;
use serde::Serialize;

use crate::db::{load_many_to_falkor, LoadOutcome};
use crate::versioning::{discover_version, latest_graph_artifacts, prune_other_versions};
use crate::{DEFAULT_FALKOR_URL, DEFAULT_GRAPH_KEY};

/// CLI overrides for community detection, process tracing, and feature grouping.
#[derive(Default)]
pub(crate) struct DiscoverOverrides {
    pub resolution: Option<f64>,
    pub min_community_size: Option<usize>,
    pub max_trace_depth: Option<usize>,
    pub max_processes: Option<usize>,
    pub max_branching: Option<usize>,
    pub min_trace_confidence: Option<f32>,
    /// Feature classification strategy: "package" (default), "structural", "hybrid".
    pub feature_strategy: String,
}

pub(crate) fn run_discover(
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

pub(crate) fn run_discover_core(
    repo: &Path,
    overrides: &DiscoverOverrides,
) -> Result<DiscoverOutcome> {
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
        fmt_count(nodes.len()),
        fmt_count(edges.len())
    ));

    // Load architecture hint from repo-map.json (written during scan/analyze).
    let arch_hint = read_architecture_hint(repo);
    tracing::info!(
        architecture_hint = ?arch_hint,
        "architecture hint loaded"
    );

    let mut community_cfg = cih_community::CommunityConfig::default();
    if cih_community::is_large_graph(&nodes) {
        // Large graphs are often sparsely connected (many unresolved external refs), so
        // keeping resolution at 1.0 avoids over-splitting already-fragmented clusters.
        // Raise min_community_size to 3 to drop 2-node fragments that aren't meaningful.
        tracing::info!(
            nodes = nodes.len(),
            resolution = 1.0,
            max_iterations = 3,
            min_community_size = 3,
            "large graph detected — using conservative resolution to reduce fragmentation"
        );
        community_cfg.max_iterations = 3;
        community_cfg.min_community_size = 3;
    }
    // Monolith hint: increase min_community_size further to fight over-fragmentation.
    // A 55-module monolith emitting 4000+ Leiden communities is over-split; raising the
    // minimum to 5 drops isolated 2-4 node fragments that have no meaningful business story.
    if arch_hint == ArchitectureHint::Monolith && community_cfg.min_community_size < 5 {
        tracing::info!(
            min_community_size = 5,
            "monolith detected — raising min_community_size to reduce over-fragmentation"
        );
        community_cfg.min_community_size = 5;
    }
    // Apply CLI overrides on top of heuristics.
    if let Some(v) = overrides.resolution {
        community_cfg.resolution = v;
    }
    if let Some(v) = overrides.min_community_size {
        community_cfg.min_community_size = v;
    }

    tracing::info!(
        resolution = community_cfg.resolution,
        min_community_size = community_cfg.min_community_size,
        "running community detection"
    );
    ui.spin("Detecting communities");
    let community_output = cih_community::detect_communities(&nodes, &edges, &community_cfg);
    tracing::info!(
        communities = community_output.nodes.len(),
        edges = community_output.edges.len(),
        "community detection complete"
    );
    ui.finish_with(format!("{} communities", fmt_count(community_output.nodes.len())));

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

    let entry_registry = cih_community::EntrypointRegistry::load(repo);
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
    ui.finish_with(format!("{} processes", fmt_count(process_output.nodes.len())));

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
        fmt_count(output_nodes.len()),
        fmt_count(output_edges.len()),
        &version[..8.min(version.len())]
    ));

    // ── Feature artifacts ─────────────────────────────────────────────────────
    let feature_strategy_kind = if overrides.feature_strategy.is_empty() {
        "package"
    } else {
        overrides.feature_strategy.as_str()
    };
    ui.spin(format!("Grouping features ({})", feature_strategy_kind));
    let pkg_cfg = PackageConfig::load_or_default(repo);
    let feature_strategy = match build_feature_strategy(feature_strategy_kind, pkg_cfg) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(
                strategy = feature_strategy_kind,
                error = %err,
                "feature strategy failed to load — falling back to package"
            );
            Box::new(cih_grouping::PackageStrategy::new(PackageConfig::load_or_default(repo)))
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
    write_feature_artifacts(&feat_dir, feature_strategy.name(), &raw_entries, &merged_entries)
        .with_context(|| {
            format!("failed to write feature artifacts to {}", feat_dir.display())
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
    ui.finish_with(format!("{} features", fmt_count(feature_count)));

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

/// Everything `run_discover_core` produced (DB-free), used to load + report.
pub(crate) struct DiscoverOutcome {
    pub(crate) source_artifacts: GraphArtifacts,
    pub(crate) artifacts: GraphArtifacts,
    pub(crate) artifacts_dir: PathBuf,
    pub(crate) version: String,
    pub(crate) route_count: usize,
    pub(crate) community_count: usize,
    pub(crate) process_count: usize,
    pub(crate) member_edge_count: usize,
    pub(crate) step_edge_count: usize,
    pub(crate) node_count: usize,
    pub(crate) edge_count: usize,
    pub(crate) feature_count: usize,
}

impl DiscoverOutcome {
    pub(crate) fn artifact_sets_for_load(&self) -> [&GraphArtifacts; 2] {
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

    fn print_human(&self, load: &LoadOutcome) {
        println!(
            "Discover: source graph {} -> {} communities, {} processes.",
            self.source_artifacts.version.0, self.community_count, self.process_count
        );
        println!(
            "Edges: {} MEMBER_OF, {} STEP_IN_PROCESS.",
            self.member_edge_count, self.step_edge_count
        );
        println!(
            "Artifacts: {} (version {})",
            self.artifacts_dir.display(),
            self.version
        );
        match load {
            LoadOutcome::Loaded(stats) => {
                println!(
                    "FalkorDB: loaded {} nodes, {} edges.",
                    stats.nodes, stats.edges
                )
            }
            LoadOutcome::Reused => println!("FalkorDB: unchanged; existing live graph reused."),
            LoadOutcome::Skipped => println!("FalkorDB: skipped (--no-load)."),
            LoadOutcome::Failed(_) => {
                println!("FalkorDB: load failed (artifacts on disk - re-run to retry).")
            }
        }
    }

    fn print_styled(&self, load: &LoadOutcome) {
        let ver = &self.version[..8.min(self.version.len())];
        crate::ui::print_header("Discover", "", Some(ver));
        crate::ui::print_row("Communities", &fmt_count(self.community_count));
        crate::ui::print_row("Processes", &fmt_count(self.process_count));
        crate::ui::print_row("Features", &fmt_count(self.feature_count));
        crate::ui::print_row(
            "Edges",
            &format!(
                "{}  member  {}  process",
                fmt_count(self.member_edge_count),
                fmt_count(self.step_edge_count)
            ),
        );
        crate::ui::print_row("Artifacts", &self.artifacts_dir.display().to_string());
        let falkor_str = match load {
            LoadOutcome::Loaded(stats) => format!(
                "{}  nodes  {}  edges",
                fmt_count(stats.nodes as usize),
                fmt_count(stats.edges as usize)
            ),
            LoadOutcome::Skipped => "\x1b[2mskipped (--no-load)\x1b[0m".to_string(),
            LoadOutcome::Reused => "\x1b[2mreused (no changes)\x1b[0m".to_string(),
            LoadOutcome::Failed(e) => format!("\x1b[31mfailed\x1b[0m  \x1b[2m{e}\x1b[0m"),
        };
        crate::ui::print_row("FalkorDB", &falkor_str);
        eprintln!();
    }
}

fn fmt_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
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

fn read_architecture_hint(repo: &Path) -> ArchitectureHint {
    let path = repo.join(".cih").join("repo-map.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return ArchitectureHint::Unknown;
    };
    serde_json::from_str::<RepoMap>(&raw)
        .map(|rm| rm.architecture_hint)
        .unwrap_or(ArchitectureHint::Unknown)
}
