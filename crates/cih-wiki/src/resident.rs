//! Resident, on-demand wiki rendering (P3.8).
//!
//! [`OwnedWiki`] holds a repo's graph + community artifacts **owned** (so it can
//! live resident in a server cache, `Send + Sync`), and renders a single page on
//! request via the pure [`render_page`] core — no batch pipeline, no `.cih/wiki/`
//! files, always fresh at the loaded `graph_version`.
//!
//! This is the graph-only tier in the default **package** grouping: it
//! reproduces exactly what `cih-engine wiki <repo>` writes for a page (mode
//! `graph` = no LLM, grouping `package` = features from package paths), by
//! assembling the same `WikiInput` (enrichment maps `None`). A read-only
//! enrichment splice is layered on top in a later step.
//!
//! `RenderContext` + `PageIndex` are rebuilt per call from the owned data (the
//! expensive `WikiGraph` build is done once, at load). Measurement: on Fineract
//! (87k nodes) the per-call rebuild is ~150ms, so a resident-`RenderContext`
//! cache is a warranted follow-up for high-throughput serving.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use cih_core::{Edge, GraphArtifacts, Node, RepoMap};

use crate::render::{build_page_index, render_page, RenderedPage};
use crate::{
    resolve_feature_groups, source_bodies, BodyEntry, EntrypointRecord, RenderContext,
    WikiGenerationInfo, WikiGraph, WikiInput,
};

/// A repo's wiki render inputs, owned so they can be cached resident and shared
/// across threads. Built once from `.cih/artifacts` (+ `artifacts-community`).
pub struct OwnedWiki {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    community_nodes: Vec<Node>,
    community_edges: Vec<Edge>,
    graph: WikiGraph,
    bodies: Arc<HashMap<String, BodyEntry>>,
    repo_map: Option<RepoMap>,
    unresolved_report: Option<String>,
    entrypoints: Vec<EntrypointRecord>,
    repo_name: String,
    graph_version: String,
    community_version: String,
}

impl OwnedWiki {
    /// Load `repo`'s graph artifacts and build the resident graph in **package
    /// grouping** — the `cih-engine wiki` default (grouping = "package"). Mirrors
    /// the package branch of `cih-engine/src/wiki/loader.rs` so live output
    /// matches the batch. `grouping` string recorded is "package"; no community
    /// artifacts are read (features come from package paths), so the batch's
    /// `Processes: 0` / package features are reproduced exactly.
    pub fn load_package_mode(repo: &Path, repo_name: String) -> Result<Self> {
        use std::sync::Arc as StdArc;

        let graph_artifacts =
            GraphArtifacts::latest_in_dir(&repo.join(".cih").join("artifacts"))
                .with_context(|| format!("no graph artifacts under {}", repo.display()))?;
        let nodes = graph_artifacts
            .read_nodes()
            .with_context(|| format!("failed to read {}", graph_artifacts.nodes_path.display()))?;
        let edges = graph_artifacts
            .read_edges()
            .with_context(|| format!("failed to read {}", graph_artifacts.edges_path.display()))?;
        let graph_version = graph_artifacts.version.to_string();

        // ── Package feature strategy (verbatim from loader.rs package branch) ──
        let pkg_cfg = cih_grouping::PackageConfig::load_or_default(repo);
        let pkg_strategy: StdArc<dyn cih_grouping::FeatureStrategy> =
            StdArc::new(cih_grouping::PackageStrategy::new(pkg_cfg));
        let repo_default_feature: StdArc<String> = StdArc::new({
            let raw = repo
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("shared")
                .to_lowercase();
            let mut s = raw.as_str();
            for suf in &[
                "-api", "-service", "-impl", "-core", "-module", "-web", "-rest",
            ] {
                s = s.strip_suffix(suf).unwrap_or(s);
            }
            for pfx in &[
                "banking-",
                "payment-",
                "finance-",
                "base-",
                "common-",
                "core-",
                "shared-",
                "platform-",
                "infra-",
                "app-",
                "service-",
            ] {
                s = s.strip_prefix(pfx).unwrap_or(s);
            }
            if s.is_empty() || s == "shared" {
                raw
            } else {
                s.to_string()
            }
        });
        let feature_lookup: StdArc<HashMap<String, String>> = StdArc::new(
            cih_grouping::find_feature_artifact_dir(repo, graph_artifacts.version.as_str())
                .and_then(|dir| cih_grouping::read_feature_artifact(&dir).ok())
                .map(|entries| entries.into_iter().map(|e| (e.node_id, e.name)).collect())
                .unwrap_or_default(),
        );

        let graph = {
            let s = pkg_strategy.clone();
            let lk = feature_lookup.clone();
            let df = repo_default_feature.clone();
            WikiGraph::build_package_grouped(&nodes, &edges, &move |node_id, f| {
                let feat = lk.get(node_id).cloned().unwrap_or_else(|| s.feature_of(f));
                if feat == "shared" {
                    df.as_ref().clone()
                } else {
                    feat
                }
            })
        };
        // Package groups act as "communities"; no community edges in package mode.
        let community_nodes: Vec<Node> = graph.community_nodes.clone();
        let community_edges: Vec<Edge> = Vec::new();
        let community_version = graph_version.clone();

        // Source bodies for community members only (matches the batch loader).
        let bodies = {
            let member_ids: std::collections::HashSet<&str> = community_nodes
                .iter()
                .flat_map(|c| {
                    graph
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
            Arc::new(source_bodies(&body_nodes, repo))
        };

        let repo_map = read_repo_map(repo);
        let unresolved_report = graph_artifacts
            .nodes_path
            .parent()
            .map(|p| p.join("unresolved-refs.md"))
            .filter(|p| p.is_file())
            .and_then(|p| std::fs::read_to_string(p).ok());
        let entrypoints = read_entrypoints(repo);

        Ok(Self {
            nodes,
            edges,
            community_nodes,
            community_edges,
            graph,
            bodies,
            repo_map,
            unresolved_report,
            entrypoints,
            repo_name,
            graph_version,
            community_version,
        })
    }

    pub fn graph_version(&self) -> &str {
        &self.graph_version
    }

    pub fn repo_name(&self) -> &str {
        &self.repo_name
    }

    /// Build a graph-only `WikiInput` borrowing this bundle's owned data. Cheap:
    /// borrows the node/edge slices, `Arc`-clones bodies, clones only small
    /// scalar fields.
    fn graph_only_input(&self) -> WikiInput<'_> {
        WikiInput {
            nodes: &self.nodes,
            edges: &self.edges,
            community_nodes: &self.community_nodes,
            community_edges: &self.community_edges,
            repo_name: self.repo_name.clone(),
            graph_version: self.graph_version.clone(),
            community_version: self.community_version.clone(),
            unresolved_report: self.unresolved_report.clone(),
            repo_map: self.repo_map.clone(),
            llm_summaries: None,
            llm_full: None,
            llm_info: None,
            module_tree: None,
            generation: WikiGenerationInfo {
                mode: "graph".to_string(),
                grouping: "package".to_string(),
                review_required: false,
                html_viewer: false,
                incremental: false,
            },
            first_module_tree: None,
            save_evidence: None,
            controller_summaries: None,
            feature_llm_summaries: None,
            flow_llm_summaries: None,
            grouping: "package".to_string(),
            filter_feature: Vec::new(),
            bodies: self.bodies.clone(),
            // Package features were baked into `graph` at load via
            // `build_package_grouped`; `feature_of` is not called during render
            // (only during that build), so a placeholder is safe here.
            feature_of: Box::new(|_, _| "shared".to_string()),
            entrypoints: self.entrypoints.clone(),
            repo_commit: None,
            flags_hash: None,
            changed_files: None,
        }
    }

    /// Render one page by slug, live. `None` when the slug isn't a known page.
    pub fn render_slug(&self, slug: &str) -> Option<RenderedPage> {
        let input = self.graph_only_input();
        let feature_groups = resolve_feature_groups(&self.graph, &input);
        let ctx = RenderContext::build(&self.graph, &input, &feature_groups);
        let index = build_page_index(&self.graph, &ctx);
        render_page(&self.graph, &ctx, &index, slug, None)
    }

    /// All addressable page slugs (for search / enumeration).
    pub fn slugs(&self) -> Vec<String> {
        let input = self.graph_only_input();
        let feature_groups = resolve_feature_groups(&self.graph, &input);
        let ctx = RenderContext::build(&self.graph, &input, &feature_groups);
        let index = build_page_index(&self.graph, &ctx);
        index.slugs().map(str::to_string).collect()
    }
}

fn read_repo_map(repo: &Path) -> Option<RepoMap> {
    let path = repo.join(".cih").join("repo-map.json");
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn read_entrypoints(repo: &Path) -> Vec<EntrypointRecord> {
    let path = repo.join(".cih").join("entrypoints.json");
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str::<Vec<EntrypointRecord>>(&raw).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}
