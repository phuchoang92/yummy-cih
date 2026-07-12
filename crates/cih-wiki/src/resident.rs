//! Resident, on-demand wiki rendering (P3.8).
//!
//! [`OwnedWiki`] holds a repo's graph **owned** (so it can live resident in a
//! server cache, `Send + Sync`) and renders a single page on request via the
//! pure [`render_page`] core — no batch pipeline, no `.cih/wiki/` files, always
//! fresh at the loaded `graph_version`.
//!
//! This is the graph-only tier in the default **package** grouping: it
//! reproduces exactly what `cih-engine wiki <repo>` writes for a page (mode
//! `graph` = no LLM, grouping `package` = features from package paths), by
//! assembling the same `WikiInput` (enrichment maps `None`). A read-only
//! enrichment splice is layered on top in a later step.
//!
//! The expensive `WikiGraph`, `RenderContext`, and `PageIndex` are all built
//! **once at load** and cached resident, so `render_slug` is a sub-millisecond
//! lookup + single-page render (no per-request ctx/index rebuild). The
//! owner → `WikiInput` → `RenderContext` borrow chain is held together with
//! `ouroboros` (`WikiInput` borrows the node/edge slices; `RenderContext`
//! borrows the graph + input).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use cih_core::{Edge, GraphArtifacts, Node, RepoMap};

use crate::render::{build_page_index, render_page, PageIndex, RenderContext, RenderedPage};
use crate::{
    resolve_feature_groups, source_bodies, BodyEntry, EntrypointRecord, WikiGenerationInfo,
    WikiGraph, WikiInput,
};

/// Self-referential resident render state: owns the graph data, and the
/// `WikiInput` + `RenderContext` that borrow it, built once.
#[ouroboros::self_referencing]
struct ResidentInner {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    community_nodes: Vec<Node>,
    community_edges: Vec<Edge>,
    graph: WikiGraph,
    #[borrows(nodes, edges, community_nodes, community_edges)]
    #[covariant]
    input: WikiInput<'this>,
    #[borrows(graph, input)]
    #[covariant]
    ctx: RenderContext<'this>,
}

/// A repo's resident wiki renderer. Cheap to render from once loaded.
pub struct OwnedWiki {
    inner: ResidentInner,
    index: PageIndex,
    repo_name: String,
    graph_version: String,
}

/// Owned `WikiInput` parts (everything except the borrowed node/edge slices),
/// moved into the resident `WikiInput` at build time.
struct InputParts {
    bodies: Arc<HashMap<String, BodyEntry>>,
    repo_map: Option<RepoMap>,
    unresolved_report: Option<String>,
    entrypoints: Vec<EntrypointRecord>,
    repo_name: String,
    graph_version: String,
    community_version: String,
}

impl OwnedWiki {
    /// Load `repo`'s graph artifacts and build the resident renderer in the
    /// **package** grouping (the `cih-engine wiki` default). Mirrors the package
    /// branch of `cih-engine/src/wiki/loader.rs` so live output matches batch:
    /// no `artifacts-community` is read, features come from package paths.
    pub fn load_package_mode(repo: &Path, repo_name: String) -> Result<Self> {
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
        let pkg_strategy: Arc<dyn cih_grouping::FeatureStrategy> =
            Arc::new(cih_grouping::PackageStrategy::new(pkg_cfg));
        let repo_default_feature: Arc<String> = Arc::new({
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
        let feature_lookup: Arc<HashMap<String, String>> = Arc::new(
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

        let parts = InputParts {
            bodies,
            repo_map,
            unresolved_report,
            entrypoints,
            repo_name: repo_name.clone(),
            graph_version: graph_version.clone(),
            community_version,
        };

        // Build the resident WikiInput + RenderContext once (self-referential).
        let inner = ResidentInnerBuilder {
            nodes,
            edges,
            community_nodes,
            community_edges,
            graph,
            input_builder: |nodes, edges, community_nodes, community_edges| {
                graph_only_input(parts, nodes, edges, community_nodes, community_edges)
            },
            ctx_builder: |graph, input| {
                let feature_groups = resolve_feature_groups(graph, input);
                RenderContext::build(graph, input, &feature_groups)
            },
        }
        .build();

        // Build the page index once from the resident graph + ctx.
        let index = build_page_index(inner.borrow_graph(), inner.borrow_ctx());

        Ok(Self {
            inner,
            index,
            repo_name,
            graph_version,
        })
    }

    pub fn graph_version(&self) -> &str {
        &self.graph_version
    }

    pub fn repo_name(&self) -> &str {
        &self.repo_name
    }

    /// Render one page by slug from the resident state. Sub-millisecond: no
    /// ctx/index rebuild. `None` when the slug isn't a known page.
    pub fn render_slug(&self, slug: &str) -> Option<RenderedPage> {
        render_page(
            self.inner.borrow_graph(),
            self.inner.borrow_ctx(),
            &self.index,
            slug,
            None,
        )
    }

    /// All addressable page slugs (for search / enumeration).
    pub fn slugs(&self) -> Vec<String> {
        self.index.slugs().map(str::to_string).collect()
    }
}

/// Assemble the graph-only `WikiInput` from the borrowed slices + owned parts.
fn graph_only_input<'a>(
    parts: InputParts,
    nodes: &'a [Node],
    edges: &'a [Edge],
    community_nodes: &'a [Node],
    community_edges: &'a [Edge],
) -> WikiInput<'a> {
    WikiInput {
        nodes,
        edges,
        community_nodes,
        community_edges,
        repo_name: parts.repo_name,
        graph_version: parts.graph_version,
        community_version: parts.community_version,
        unresolved_report: parts.unresolved_report,
        repo_map: parts.repo_map,
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
        bodies: parts.bodies,
        // Package features were baked into `graph` at load via
        // `build_package_grouped`; `feature_of` is not called during render.
        feature_of: Box::new(|_, _| "shared".to_string()),
        entrypoints: parts.entrypoints,
        repo_commit: None,
        flags_hash: None,
        changed_files: None,
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
