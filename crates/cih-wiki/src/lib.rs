//! Wiki page generation from graph artifacts.
//!
//! # Error philosophy
//!
//! This is leaf orchestration code consumed only by the `cih-engine` binary,
//! which reports errors rather than branching on them — so functions return
//! `anyhow::Result` by design (context strings over structured variants).

pub mod bodies;
pub mod enrich_maps;
pub mod features;
pub mod graph;
pub mod html;
pub mod manifest;
pub mod mermaid;
pub mod module_tree;
pub mod pages;
pub mod render;
pub mod resident;
pub mod sink;
pub mod slugify;

pub use bodies::{source_bodies, BodyEntry};
pub use cih_core::RepoMap;
pub use enrich_maps::{build_class_maps, class_method_chains};
pub use features::{assign_class_slugs, FeatureGroup};
pub use graph::WikiGraph;
pub use manifest::{NavEntry, PageEntry, WikiGenerationInfo, WikiLlmInfo, WikiManifest, WikiStats};
pub use module_tree::{
    build_graph_module_tree, build_wiki_meta, read_module_tree, validate_module_tree,
    ClassCacheEntry, ClassEnrichmentStore, CommunityFullCacheEntry, FeatureMetaEntry,
    FlowCacheEntry, ModuleTreeSource, WikiMeta, WikiModuleCacheEntry, WikiModuleNode,
    WikiModuleTree,
};
pub use render::{
    build_page_index, render_page, resolve_slug, PageIndex, PageSubject, RenderContext,
    RenderedPage,
};
pub use resident::OwnedWiki;

use sink::PageSink;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Result;
use cih_core::{Node, NodeId, NodeKind, Range};
use features::{group_communities_by_feature, group_nodes_by_package};
use graph::node_stereotype;
use slugify::slugify;

/// Scheduled job or event-listener method detected during `discover`.
/// Loaded from `.cih/entrypoints.json` and threaded into wiki generation.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct EntrypointRecord {
    pub method_id: String,
    /// "scheduled" or "event_listener"
    pub kind: String,
    pub topics: Vec<String>,
}

/// Pre-computed short AI summaries for one community (all three roles).
/// Used in `llm-summary` mode. Produced by `enrich_communities()`; passed into `WikiInput`.
#[derive(Clone, Debug, Default)]
pub struct CommunityLlmSummary {
    /// 2-3 sentences in plain business language.
    pub po: String,
    /// 2-3 sentences on workflows, contracts, and events.
    pub ba: String,
    /// 2-3 sentences on technical structure.
    pub dev: String,
}

/// LLM-generated metadata for one controller class.
/// Produced by `enrich_controllers()` in `cih-engine`; passed into `WikiInput`.
#[derive(Clone, Debug, Default)]
pub struct ControllerLlmSummary {
    /// 1-2 sentences in plain business language describing what this controller handles.
    pub description: String,
    /// Business domain slug inferred by LLM (e.g. "payment"). Applied only when the
    /// file-path heuristic returns "shared", to move the controller to the right feature.
    pub feature: Option<String>,
    /// Per-handler-method descriptions keyed by Java method name (e.g. "addItemToMyCart").
    pub method_descriptions: HashMap<String, String>,
}

/// LLM-generated feature-level overview for PO and BA pages.
/// One per wiki feature (module); produced by the feature-enrichment pass in `cih-engine`.
#[derive(Clone, Debug, Default)]
pub struct FeatureLlmSummary {
    /// 3-5 sentence plain-language business overview for the whole feature.
    pub po_overview: String,
    /// Bullet list of capabilities.
    pub po_capabilities: String,
    /// 3-5 sentence process overview for business analysts.
    pub ba_process_overview: String,
    /// Key business rules / invariants observed in the evidence.
    pub ba_business_rules: String,
}

/// LLM-generated summary for a single process trace (flow).
/// One per process_id; produced by the per-flow enrichment pass in `cih-engine`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FlowLlmSummary {
    /// 2-3 sentence narrative of the full flow for BA pages.
    pub narrative: String,
    /// 1-2 sentence business impact for PO pages.
    pub business_impact: String,
    /// One sentence per step, indexed by step_number - 1 (0-based).
    pub step_descriptions: Vec<String>,
}

/// Richer per-community LLM content for `llm-full` mode.
/// Each field is a markdown string that is inserted into the relevant page section.
#[derive(Clone, Debug, Default)]
pub struct CommunityLlmFull {
    // PO page sections
    pub po_summary: String,
    pub po_capabilities: String,
    pub po_workflows: String,
    pub po_open_questions: String,
    // BA page sections
    pub ba_process_overview: String,
    pub ba_contracts: String,
    pub ba_business_rules: String,
    // Dev page sections
    pub dev_responsibility: String,
    pub dev_key_classes: String,
    pub dev_entry_points: String,
}

/// Maps `(node_id, file_path)` to a feature name.
pub type FeatureOfFn = Box<dyn Fn(&str, &str) -> String + Send + Sync>;

pub struct WikiInput<'a> {
    pub nodes: &'a [Node],
    pub edges: &'a [cih_core::Edge],
    pub community_nodes: &'a [Node],
    pub community_edges: &'a [cih_core::Edge],
    pub repo_name: String,
    pub graph_version: String,
    pub community_version: String,
    /// Contents of `unresolved-refs.md` if present.
    pub unresolved_report: Option<String>,
    pub repo_map: Option<RepoMap>,
    /// Keyed by community_id. `None` = graph-only mode. Used in `llm-summary` mode.
    pub llm_summaries: Option<HashMap<String, CommunityLlmSummary>>,
    /// Keyed by community_id. Used in `llm-full` mode alongside `llm_summaries`.
    pub llm_full: Option<HashMap<String, CommunityLlmFull>>,
    /// LLM run metadata, recorded in the manifest when enrichment was requested.
    pub llm_info: Option<WikiLlmInfo>,
    /// Accepted module tree. If omitted, a deterministic graph-derived tree is built.
    pub module_tree: Option<WikiModuleTree>,
    /// Generation metadata recorded in the manifest.
    pub generation: WikiGenerationInfo,
    /// Optional first LLM-proposed tree, kept for review/reproducibility.
    pub first_module_tree: Option<WikiModuleTree>,
    /// Per-community evidence packs to save to .cih/wiki/evidence/ (--save-evidence).
    pub save_evidence: Option<HashMap<String, String>>,
    /// Keyed by controller class name. Populated when LLM enrichment is active.
    pub controller_summaries: Option<HashMap<String, ControllerLlmSummary>>,
    /// Keyed by feature name. One entry per wiki feature from the feature-enrichment LLM pass.
    pub feature_llm_summaries: Option<HashMap<String, FeatureLlmSummary>>,
    /// Keyed by process_id. One entry per flow from the per-flow enrichment LLM pass.
    pub flow_llm_summaries: Option<HashMap<String, FlowLlmSummary>>,
    /// Grouping strategy: "package" (by Java package path) or "graph"/"llm" (Leiden communities).
    pub grouping: String,
    /// Only generate pages for features whose name contains one of these substrings
    /// (case-insensitive). Empty = no filter.
    pub filter_feature: Vec<String>,
    /// Stripped source bodies keyed by node_id string. Empty map = no bodies shown.
    /// `Arc` so the on-demand render path (P3.8) can build a fresh `WikiInput`
    /// per request without cloning the whole (large) map.
    pub bodies: std::sync::Arc<HashMap<String, BodyEntry>>,
    /// Maps `(node_id, file)` to a feature slug. Supplied by `cih-engine`; called during
    /// `WikiGraph::build_package_grouped`. When grouping is "graph"/"llm" never called.
    /// When a pre-computed artifact is available, `node_id` gives a direct lookup;
    /// otherwise fall back to file-path heuristics.
    pub feature_of: FeatureOfFn,
    /// Scheduled jobs and event listeners from `.cih/entrypoints.json`.
    /// Empty when the sidecar does not exist (no such methods in the repo).
    pub entrypoints: Vec<EntrypointRecord>,
    /// Current git HEAD SHA of the target repo, stamped into wiki_meta.json for the no-op gate.
    pub repo_commit: Option<String>,
    /// FNV-1a hash of effective wiki flags (mode‖grouping‖language‖model‖PROMPT_VERSION).
    /// Stored in wiki_meta.json so the no-op gate can detect flag changes between runs.
    pub flags_hash: Option<String>,
    /// Files changed since the `--since <ref>` git ref.
    /// When `Some`, only features with nodes in these files are re-rendered.
    /// `None` = full render (default).
    pub changed_files: Option<std::collections::HashSet<String>>,
}

#[derive(Debug)]
pub struct WikiOutcome {
    pub out_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub agent_index_path: PathBuf,
    pub page_count: usize,
    pub community_count: usize,
    pub route_count: usize,
    pub llm_enriched: bool,
    /// Pages actually written to disk (new or content changed).
    pub pages_written: usize,
    /// Pages skipped because content was identical to the existing file.
    pub pages_unchanged: usize,
}

/// Strip redundant class/method references that the LLM sometimes prepends to descriptions.
/// Handles patterns like `ClassName.methodName/N()`, "The ClassName ...", `` `methodName` ``.
pub(crate) fn clean_method_desc(desc: &str, cls: &str, meth: &str) -> String {
    let mut s = desc.trim().to_string();

    // General scan: if ClassName.method or `ClassName`.method appears anywhere in the
    // first 80 chars of the text, strip from the start up to and including that
    // signature (ends after the first ')' or after the signature word).
    // This handles patterns like:
    //   "ClassName.method/N() verb..."
    //   "The ClassName.method/N() verb..."
    //   "The resource method ClassName.method/N() is called..."
    let sig_needle = format!("{}.", cls);
    let sig_needle_bt = format!("`{}`.", cls);
    let scan_window = s.floor_char_boundary(100);
    let sig_pos_len = s[..scan_window]
        .find(sig_needle_bt.as_str())
        .map(|p| (p, sig_needle_bt.len()))
        .or_else(|| {
            s[..scan_window]
                .find(sig_needle.as_str())
                .map(|p| (p, sig_needle.len()))
        });
    if let Some((pos, needle_len)) = sig_pos_len {
        // Find the end of the signature: scan past `()` and any trailing space/punctuation
        let after_start = (pos + needle_len).min(s.len());
        let after_sig = &s[after_start..];
        // Find closing ')' — the signature ends there; take everything after it
        if let Some(paren_close) = after_sig.find(')') {
            let rest = after_sig[paren_close + 1..].trim_start_matches([' ', '\n']);
            if !rest.is_empty() {
                // Drop connective phrases like "is called to", "is invoked to", "resource method"
                let rest = rest
                    .strip_prefix("is called to ")
                    .or_else(|| rest.strip_prefix("is invoked to "))
                    .or_else(|| rest.strip_prefix("resource method "))
                    .unwrap_or(rest);
                s = rest.trim_start().to_string();
            }
        } else {
            // No closing paren — just take everything after the class name
            let rest = after_sig
                .find(' ')
                .map(|i| after_sig[i..].trim_start())
                .unwrap_or("");
            if !rest.is_empty() {
                s = rest.to_string();
            }
        }
    }

    // Strip leading backtick-quoted method name (e.g. "`createDelinquencyBucket` is called to...")
    let bt_meth = format!("`{}` ", meth);
    if let Some(rest) = s.strip_prefix(&bt_meth) {
        s = rest.to_string();
    }

    // Capitalise first letter
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Resolve the feature-group hierarchy the wiki is built around: Leiden
/// communities (graph/llm) or Java packages (package mode), with synthesis from
/// controller file paths when no communities exist, then the `--filter-feature`
/// filter. Shared by `generate_wiki` and the standalone render path so both see
/// the same feature set.
pub fn resolve_feature_groups(graph: &WikiGraph, input: &WikiInput<'_>) -> Vec<FeatureGroup> {
    let mut feature_groups = if input.grouping == "package" {
        // Restrict to packages that survived --filter-route (stored in input.community_nodes).
        // When no route filter was active, input.community_nodes contains all packages.
        let allowed_ids: std::collections::HashSet<&str> = input
            .community_nodes
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        let all_groups = group_nodes_by_package(graph);
        if allowed_ids.is_empty() {
            all_groups
        } else {
            all_groups
                .into_iter()
                .filter(|g| {
                    g.community_ids
                        .iter()
                        .any(|id| allowed_ids.contains(id.as_str()))
                })
                .collect()
        }
    } else {
        group_communities_by_feature(graph)
    };

    // No communities (discover not run): synthesize one group per feature found in
    // controller file paths so controller pages still get generated.
    if feature_groups.is_empty() && !graph.routes_by_controller.is_empty() {
        let mut features: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for feat in graph.controller_feature.values() {
            features.insert(feat.clone());
        }
        feature_groups = features
            .into_iter()
            .map(|feature| FeatureGroup {
                feature,
                community_ids: vec![],
            })
            .collect();
    }

    // Apply --filter-feature: keep only groups whose name contains a filter substring.
    if !input.filter_feature.is_empty() {
        let filters: Vec<String> = input
            .filter_feature
            .iter()
            .map(|f| f.to_lowercase())
            .collect();
        feature_groups.retain(|g| {
            let name = g.feature.to_lowercase();
            filters.iter().any(|f| name.contains(f.as_str()))
        });
    }
    feature_groups
}

/// Collected page output from one generation phase (pages + nav entries).
struct PageBatch {
    pages: Vec<PageEntry>,
    nav: BTreeMap<String, Vec<NavEntry>>,
}

impl PageBatch {
    fn new() -> Self {
        Self {
            pages: Vec::new(),
            nav: BTreeMap::new(),
        }
    }
}

/// Write the system-level global pages: system index + shared routes.
fn emit_global_pages(
    feature_groups: &[FeatureGroup],
    graph: &WikiGraph,
    repo_name: &str,
    sink: &mut PageSink,
) -> Result<PageBatch> {
    let mut batch = PageBatch::new();

    let system_md = pages::system_index::render_system_index(feature_groups, graph, repo_name);
    sink.push("pages/index.md", system_md);
    batch.pages.push(PageEntry {
        slug: "index".into(),
        role: "system".into(),
        title: repo_name.to_string(),
        kind: "index".into(),
        path: "pages/index.md".into(),
        json_path: None,
        community_id: None,
    });

    let routes_md = pages::shared::render_routes_page(graph);
    let routes_json = pages::shared::render_routes_json(graph);
    sink.push("pages/routes.md", routes_md);
    sink.push(
        "pages/routes.json",
        serde_json::to_string_pretty(&routes_json)?,
    );
    batch.pages.push(PageEntry {
        slug: "routes".into(),
        role: "shared".into(),
        title: "API Routes".into(),
        kind: "routes".into(),
        path: "pages/routes.md".into(),
        json_path: Some("pages/routes.json".into()),
        community_id: None,
    });

    Ok(batch)
}

/// Write all pages for one feature: index, PO, BA, per-class dev, and per-route API flow.
/// `class_dev_slugs` is populated during dev-class generation and read by API-flow generation.
/// Register one generated page in a feature section: its sidebar nav entry plus
/// its manifest `PageEntry`. Both share `slug`/`title`/`kind`; `role` is the
/// feature. Extracted because the D1–D5 blocks below repeat this push verbatim.
fn register_page(
    batch: &mut PageBatch,
    feature: &str,
    slug: String,
    title: String,
    kind: &str,
    path: String,
    json_path: Option<String>,
) {
    batch
        .nav
        .entry(feature.to_string())
        .or_default()
        .push(NavEntry {
            slug: slug.clone(),
            title: title.clone(),
            kind: kind.into(),
        });
    batch.pages.push(PageEntry {
        slug,
        role: feature.to_string(),
        title,
        kind: kind.into(),
        path,
        json_path,
        community_id: None,
    });
}

fn emit_feature_section(
    group: &FeatureGroup,
    graph: &WikiGraph,
    ctx: &RenderContext<'_>,
    out_dir: &Path,
    class_dev_slugs: &mut HashMap<String, String>,
    sink: &mut PageSink,
    dev_entries: &mut Vec<(String, String, String)>,
) -> Result<PageBatch> {
    let feature = &group.feature;
    // Guard: feature names are used as filesystem path segments; they must only contain
    // safe slug characters ([a-z0-9-]) so that a malformed graph value cannot write
    // outside the wiki output directory.
    anyhow::ensure!(
        is_safe_page_slug(feature),
        "unsafe feature name rejected as write-path segment: {:?}",
        feature
    );
    let fctx = match ctx.feature(feature) {
        Some(fctx) => fctx,
        // No context for this feature (not in feature_groups) — nothing to emit.
        None => return Ok(PageBatch::new()),
    };
    let cids = &fctx.community_ids;
    let feature_class_set = &fctx.class_set;
    let slug_for = &fctx.slug_for;
    let class_dev_links = &fctx.class_dev_links;
    let mut batch = PageBatch::new();

    // Provenance metadata shared by all pages in this feature section.
    let feature_llm = ctx
        .input
        .feature_llm_summaries
        .as_ref()
        .and_then(|m| m.get(feature.as_str()));
    let page_meta = pages::WikiPageMeta {
        enrichment_tier: ctx.enrichment_tier,
        graph_version: &ctx.input.graph_version,
    };

    // D1 — Feature landing index
    let idx_md = pages::feature_index::render_feature_index(
        feature,
        cids,
        class_dev_links,
        graph,
        &page_meta,
    );
    sink.push(format!("pages/{}/index.md", feature), idx_md);
    register_page(
        &mut batch,
        feature,
        format!("{}/index", feature),
        format!("{} Overview", capitalize(feature)),
        "index",
        format!("pages/{}/index.md", feature),
        None,
    );

    // D2 — Feature PO (feature_llm, enrichment_tier, page_meta computed above)
    let po_md = pages::feature_po::render_feature_po(
        feature,
        cids,
        graph,
        ctx.input.llm_summaries.as_ref(),
        ctx.input.llm_full.as_ref(),
        feature_llm,
        ctx.input.flow_llm_summaries.as_ref(),
        ctx.feature_scheduled_counts
            .get(feature.as_str())
            .copied()
            .unwrap_or(0),
        ctx.feature_listener_counts
            .get(feature.as_str())
            .copied()
            .unwrap_or(0),
        &page_meta,
    );
    sink.push(format!("pages/{}/po.md", feature), po_md);
    register_page(
        &mut batch,
        feature,
        format!("{}/po", feature),
        format!("{} — Business Overview", capitalize(feature)),
        "po",
        format!("pages/{}/po.md", feature),
        None,
    );

    // D3 — Feature BA
    let ba_md = pages::feature_ba::render_feature_ba(
        feature,
        cids,
        graph,
        ctx.input.llm_summaries.as_ref(),
        ctx.input.llm_full.as_ref(),
        feature_llm,
        ctx.input.flow_llm_summaries.as_ref(),
        &page_meta,
    );
    sink.push(format!("pages/{}/ba.md", feature), ba_md);
    register_page(
        &mut batch,
        feature,
        format!("{}/ba", feature),
        format!("{} — Business Analysis", capitalize(feature)),
        "ba",
        format!("pages/{}/ba.md", feature),
        None,
    );

    // D4 — Per-class dev pages (feature_class_set and slug_for pre-computed above)
    for class_id in feature_class_set {
        let slug = slug_for
            .get(class_id.as_str())
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        let page_path = format!("{}/dev/{}", feature, slug);
        class_dev_slugs.insert(class_id.clone(), slug.clone());

        let synthesized;
        let cls_node: &Node = match graph.nodes_by_id.get(class_id.as_str()) {
            Some(n) => n,
            None => {
                let simple_name = class_id
                    .trim_start_matches("Class:")
                    .rsplit('.')
                    .next()
                    .unwrap_or("Unknown")
                    .to_string();
                let file = graph
                    .methods_by_class
                    .get(class_id.as_str())
                    .and_then(|ms| ms.first())
                    .map(|m| m.file.clone())
                    .unwrap_or_default();
                synthesized = Node {
                    id: NodeId::new(class_id.clone()),
                    kind: NodeKind::Class,
                    name: simple_name,
                    qualified_name: None,
                    file,
                    range: Range::default(),
                    props: None,
                };
                &synthesized
            }
        };
        let md = pages::dev::render_dev_class(
            graph,
            cls_node,
            &ctx.input.bodies,
            &ctx.method_flow_desc,
            &page_meta,
        );
        let json_val = pages::dev::render_dev_class_json(graph, cls_node);
        sink.push(format!("pages/{}.md", page_path), md);
        sink.push(
            format!("pages/{}.json", page_path),
            serde_json::to_string_pretty(&json_val)?,
        );
        dev_entries.push((
            class_id.clone(),
            cls_node.file.clone(),
            format!("pages/{}.md", page_path),
        ));
        let dev_title = cls_node.name.clone();
        register_page(
            &mut batch,
            feature,
            page_path.clone(),
            dev_title,
            "dev",
            format!("pages/{}.md", page_path),
            Some(format!("pages/{}.json", page_path)),
        );
    }

    // D5 — Per-route API-flow pages (controllers pre-resolved in FeatureContext)
    let feature_controllers = &fctx.controllers;

    if !feature_controllers.is_empty() {
        let api_dir = out_dir.join(format!("pages/{}/api", feature));
        std::fs::create_dir_all(&api_dir)?;
        std::fs::write(
            api_dir.join("_category_.json"),
            "{\"position\": 3, \"label\": \"API Surface\"}\n",
        )?;
        for (ctrl_pos, ctrl_name) in feature_controllers.iter().enumerate() {
            let routes = match graph.routes_by_controller.get(ctrl_name) {
                Some(routes) => routes,
                None => continue,
            };
            let ctrl_slug = slugify(ctrl_name);
            let display_title = pages::feature_po::controller_display_name(ctrl_name);
            let ctrl_summary = ctx
                .input
                .controller_summaries
                .as_ref()
                .and_then(|m| m.get(ctrl_name));
            let description = ctrl_summary
                .map(|s| s.description.as_str())
                .filter(|s| !s.is_empty());
            let empty_methods = HashMap::new();
            let method_descriptions = ctrl_summary
                .map(|s| &s.method_descriptions)
                .unwrap_or(&empty_methods);

            let stale = api_dir.join(format!("{}.md", ctrl_slug));
            let _ = std::fs::remove_file(&stale);

            let ctrl_dir = api_dir.join(&ctrl_slug);
            std::fs::create_dir_all(&ctrl_dir)?;
            std::fs::write(
                ctrl_dir.join("_category_.json"),
                format!(
                    "{{\"position\": {}, \"label\": \"{}\", \"collapsible\": true, \"collapsed\": false}}\n",
                    ctrl_pos + 1,
                    display_title
                ),
            )?;

            let ctrl_md = pages::feature_po::render_controller_page(
                ctrl_name,
                routes,
                description,
                method_descriptions,
            );
            sink.push(
                format!("pages/{}/api/{}/index.md", feature, ctrl_slug),
                ctrl_md,
            );

            for (route_pos, (handler, route)) in routes.iter().enumerate() {
                let handler_slug = pages::api_flow::handler_slug(handler.id.as_str());
                let process_id = ctx.process_by_handler.get(handler.id.as_str());
                let flow_summary = process_id
                    .and_then(|pid| ctx.input.flow_llm_summaries.as_ref()?.get(pid.as_str()))
                    .or_else(|| {
                        ctx.input
                            .flow_llm_summaries
                            .as_ref()?
                            .get(handler.id.as_str())
                    });
                let flow_md = pages::api_flow::render_api_flow_page(
                    handler,
                    route,
                    route_pos + 1,
                    flow_summary,
                    graph,
                    class_dev_slugs,
                    &ctx.method_flow_desc,
                );
                let page_path = format!("{}/api/{}/{}", feature, ctrl_slug, handler_slug);
                sink.push(format!("pages/{}.md", page_path), flow_md);
                let flow_title = pages::api_flow::handler_title(handler.id.as_str());
                register_page(
                    &mut batch,
                    feature,
                    page_path.clone(),
                    flow_title,
                    "api-flow",
                    format!("pages/{}.md", page_path),
                    None,
                );
            }
        }
    }

    Ok(batch)
}

/// Write scheduled-job and event-listener pages for all features.
fn emit_entrypoint_section(
    graph: &WikiGraph,
    ctx: &RenderContext<'_>,
    out_dir: &Path,
    class_dev_slugs: &HashMap<String, String>,
    sink: &mut PageSink,
) -> Result<PageBatch> {
    let mut batch = PageBatch::new();
    if ctx.input.entrypoints.is_empty() {
        return Ok(batch);
    }

    let all_method_desc: HashMap<String, String> = ctx
        .input
        .controller_summaries
        .iter()
        .flat_map(|m| m.values())
        .flat_map(|s| s.method_descriptions.iter())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let mut by_feature_scheduled: BTreeMap<String, Vec<&crate::EntrypointRecord>> = BTreeMap::new();
    let mut by_feature_events: BTreeMap<String, Vec<&crate::EntrypointRecord>> = BTreeMap::new();

    for ep in &ctx.input.entrypoints {
        let file = graph
            .nodes_by_id
            .get(ep.method_id.as_str())
            .map(|n| n.file.as_str())
            .unwrap_or("");
        let feature = (ctx.input.feature_of)(ep.method_id.as_str(), file);
        match ep.kind.as_str() {
            "scheduled" => by_feature_scheduled.entry(feature).or_default().push(ep),
            "event_listener" => by_feature_events.entry(feature).or_default().push(ep),
            _ => {}
        }
    }

    for (feature, entries) in &by_feature_scheduled {
        let api_dir = out_dir.join(format!("pages/{}/api", feature));
        std::fs::create_dir_all(&api_dir)?;
        let cat_path = api_dir.join("_category_.json");
        if !cat_path.exists() {
            std::fs::write(&cat_path, "{\"position\": 3, \"label\": \"API Surface\"}\n")?;
        }
        let sched_dir = api_dir.join("scheduled");
        std::fs::create_dir_all(&sched_dir)?;
        std::fs::write(
            sched_dir.join("_category_.json"),
            "{\"label\": \"Scheduled Jobs\", \"collapsible\": true, \"collapsed\": false}\n",
        )?;
        for (pos, ep) in entries.iter().enumerate() {
            let slug = pages::api_flow::handler_slug(ep.method_id.as_str());
            let md = pages::api_flow::render_scheduled_flow_page(
                ep.method_id.as_str(),
                pos + 1,
                graph,
                class_dev_slugs,
                &all_method_desc,
            );
            let page_path = format!("{}/api/scheduled/{}", feature, slug);
            sink.push(format!("pages/{}.md", page_path), md);
            let flow_title = pages::api_flow::handler_title(ep.method_id.as_str());
            batch
                .nav
                .entry(feature.clone())
                .or_default()
                .push(NavEntry {
                    slug: page_path.clone(),
                    title: flow_title.clone(),
                    kind: "scheduled-flow".into(),
                });
            batch.pages.push(PageEntry {
                slug: page_path.clone(),
                role: feature.clone(),
                title: flow_title,
                kind: "scheduled-flow".into(),
                path: format!("pages/{}.md", page_path),
                json_path: None,
                community_id: None,
            });
        }
    }

    for (feature, entries) in &by_feature_events {
        let api_dir = out_dir.join(format!("pages/{}/api", feature));
        std::fs::create_dir_all(&api_dir)?;
        let cat_path = api_dir.join("_category_.json");
        if !cat_path.exists() {
            std::fs::write(&cat_path, "{\"position\": 3, \"label\": \"API Surface\"}\n")?;
        }
        let events_dir = api_dir.join("events");
        std::fs::create_dir_all(&events_dir)?;
        std::fs::write(
            events_dir.join("_category_.json"),
            "{\"label\": \"Event Listeners\", \"collapsible\": true, \"collapsed\": false}\n",
        )?;
        for (pos, ep) in entries.iter().enumerate() {
            let slug = pages::api_flow::handler_slug(ep.method_id.as_str());
            let md = pages::api_flow::render_listener_flow_page(
                ep.method_id.as_str(),
                ep.topics.as_slice(),
                pos + 1,
                graph,
                class_dev_slugs,
                &all_method_desc,
            );
            let page_path = format!("{}/api/events/{}", feature, slug);
            sink.push(format!("pages/{}.md", page_path), md);
            let flow_title = pages::api_flow::handler_title(ep.method_id.as_str());
            batch
                .nav
                .entry(feature.clone())
                .or_default()
                .push(NavEntry {
                    slug: page_path.clone(),
                    title: flow_title.clone(),
                    kind: "listener-flow".into(),
                });
            batch.pages.push(PageEntry {
                slug: page_path.clone(),
                role: feature.clone(),
                title: flow_title,
                kind: "listener-flow".into(),
                path: format!("pages/{}.md", page_path),
                json_path: None,
                community_id: None,
            });
        }
    }

    Ok(batch)
}

/// Write community-level pages: community index and per-community detail/PO/BA pages.
fn emit_community_section(
    graph: &WikiGraph,
    ctx: &RenderContext<'_>,
    out_dir: &Path,
    sink: &mut PageSink,
) -> Result<PageBatch> {
    let mut batch = PageBatch::new();
    let comm_slug_map = slugify::build_slug_map(&graph.community_nodes);
    std::fs::create_dir_all(out_dir.join("pages/communities"))?;

    let comm_idx =
        pages::community::render_community_index(&graph.community_nodes, &comm_slug_map, graph);
    sink.push("pages/communities/index.md", comm_idx);
    batch.pages.push(PageEntry {
        slug: "communities/index".into(),
        role: "communities".into(),
        title: "Communities".into(),
        kind: "index".into(),
        path: "pages/communities/index.md".into(),
        json_path: None,
        community_id: None,
    });

    for comm in &graph.community_nodes {
        let comm_id = comm.id.as_str().to_string();
        let dir_name = comm_slug_map
            .get(&comm_id)
            .cloned()
            .unwrap_or_else(|| slugify(comm.id.as_str()));
        let dir = out_dir.join(format!("pages/communities/{dir_name}"));
        std::fs::create_dir_all(&dir)?;

        let processes_here: Vec<&Node> = graph
            .process_nodes
            .iter()
            .filter(|p| {
                p.props
                    .as_ref()
                    .and_then(|props| props.get("communities"))
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().any(|x| x.as_str() == Some(comm_id.as_str())))
                    .unwrap_or(false)
            })
            .collect();

        let llm = ctx
            .input
            .llm_summaries
            .as_ref()
            .and_then(|m| m.get(&comm_id));
        let llm_full = ctx.input.llm_full.as_ref().and_then(|m| m.get(&comm_id));

        let detail_md =
            pages::community::render_community_detail(comm, graph, &processes_here, llm);
        sink.push(format!("pages/communities/{dir_name}/index.md"), detail_md);
        batch.pages.push(PageEntry {
            slug: format!("communities/{dir_name}/index"),
            role: "communities".into(),
            title: comm.name.clone(),
            kind: "index".into(),
            path: format!("pages/communities/{dir_name}/index.md"),
            json_path: None,
            community_id: Some(comm_id.clone()),
        });

        if let Some(full) = llm_full {
            let po_md = pages::community::render_community_po(comm, graph, full);
            sink.push(format!("pages/communities/{dir_name}/po.md"), po_md);
            batch.pages.push(PageEntry {
                slug: format!("communities/{dir_name}/po"),
                role: "communities".into(),
                title: format!("{} — Business Overview", comm.name),
                kind: "po".into(),
                path: format!("pages/communities/{dir_name}/po.md"),
                json_path: None,
                community_id: Some(comm_id.clone()),
            });

            let ba_md = pages::community::render_community_ba(comm, graph, &processes_here, full);
            sink.push(format!("pages/communities/{dir_name}/ba.md"), ba_md);
            batch.pages.push(PageEntry {
                slug: format!("communities/{dir_name}/ba"),
                role: "communities".into(),
                title: format!("{} — Business Analysis", comm.name),
                kind: "ba".into(),
                path: format!("pages/communities/{dir_name}/ba.md"),
                json_path: None,
                community_id: Some(comm_id.clone()),
            });
        }
    }

    Ok(batch)
}

pub fn generate_wiki(mut input: WikiInput<'_>, out_dir: &Path) -> Result<WikiOutcome> {
    let graph = if input.grouping == "package" {
        WikiGraph::build_package_grouped(input.nodes, input.edges, &*input.feature_of)
    } else {
        WikiGraph::build(
            input.nodes,
            input.edges,
            input.community_nodes,
            input.community_edges,
        )
    };

    let unresolved_count = count_unresolved_refs(input.unresolved_report.as_deref());
    let class_count: usize = graph.community_class_counts.values().sum();
    let test_class_count = count_test_classes(&graph);
    let llm_enriched = input.llm_summaries.is_some() || input.llm_full.is_some();

    // Save evidence packs to disk if requested.
    if let Some(evidence_map) = &input.save_evidence {
        let ev_dir = out_dir.join("evidence");
        std::fs::create_dir_all(&ev_dir)?;
        for (comm_id, pack_text) in evidence_map {
            let slug = comm_id
                .replace("Community:", "community-")
                .replace([':', '/'], "-");
            std::fs::write(
                ev_dir.join(format!("{}.json", slug)),
                serde_json::to_string_pretty(&serde_json::json!({
                    "community_id": comm_id,
                    "evidence": pack_text,
                }))?,
            )?;
        }
    }

    // Feature grouping — the core of the new hierarchy.
    let feature_groups = resolve_feature_groups(&graph, &input);

    let mut module_tree = input.module_tree.take().unwrap_or_else(|| {
        build_graph_module_tree(
            &graph,
            input.repo_map.as_ref(),
            &input.graph_version,
            &input.community_version,
            input.repo_commit.clone(),
        )
    });
    // For user-provided trees that predate HEAD stamping, fill in the current commit.
    if module_tree.repo_commit.is_none() {
        module_tree.repo_commit = input.repo_commit.clone();
    }
    validate_module_tree(&module_tree, &graph)?;
    let mut wiki_meta = build_wiki_meta(
        &module_tree,
        input.llm_info.as_ref().map(|info| info.model.clone()),
        input.llm_info.as_ref().map(|info| info.language.clone()),
    );
    wiki_meta.flags_hash = input.flags_hash.clone();

    // Create directories
    std::fs::create_dir_all(out_dir)?;
    // Readers treat this marker as an unavailable, retryable publication. It
    // remains after a failed generation so no reader can combine a previous
    // manifest with partially replaced pages; a successful rerun clears it.
    let publishing_marker = out_dir.join(".publishing");
    std::fs::write(&publishing_marker, input.graph_version.as_bytes())?;
    std::fs::write(
        out_dir.join("module_tree.json"),
        serde_json::to_string_pretty(&module_tree)?,
    )?;
    if let Some(first_tree) = &input.first_module_tree {
        std::fs::write(
            out_dir.join("first_module_tree.json"),
            serde_json::to_string_pretty(first_tree)?,
        )?;
    }
    std::fs::write(
        out_dir.join("wiki_meta.json"),
        serde_json::to_string_pretty(&wiki_meta)?,
    )?;
    std::fs::create_dir_all(out_dir.join("pages"))?;
    for group in &feature_groups {
        let dev_dir = out_dir.join(format!("pages/{}/dev", group.feature));
        std::fs::create_dir_all(&dev_dir)?;
        // Feature folder root: position 10 so it sorts after index (1) and routes (2).
        let feature_dir = out_dir.join(format!("pages/{}", group.feature));
        std::fs::write(
            feature_dir.join("_category_.json"),
            format!(
                "{{\"position\": 10, \"label\": \"{}\"}}\n",
                capitalize(&group.feature)
            ),
        )?;
        std::fs::write(
            dev_dir.join("_category_.json"),
            "{\"position\": 4, \"label\": \"Technical Reference\"}\n",
        )?;
    }

    let mut page_count = 0usize;
    let mut all_pages: Vec<PageEntry> = Vec::new();
    let mut nav: BTreeMap<String, Vec<NavEntry>> = BTreeMap::new();

    let stats = WikiStats {
        community_count: graph.community_nodes.len(),
        route_count: graph.routes.len(),
        process_count: graph.process_nodes.len(),
        class_count,
        test_class_count,
        unresolved_ref_count: unresolved_count,
        feature_count: feature_groups.len(),
    };

    if input.generation.review_required {
        let manifest = WikiManifest {
            schema_version: 1,
            generated_at: cih_core::now_rfc3339(),
            repo_name: input.repo_name,
            graph_version: input.graph_version,
            community_version: input.community_version,
            stats,
            roles: vec!["po".into(), "ba".into(), "dev".into()],
            nav,
            pages: all_pages,
            llm: input.llm_info,
            generation: Some(input.generation),
            module_tree_path: Some("module_tree.json".into()),
            wiki_meta_path: Some("wiki_meta.json".into()),
            warnings: Vec::new(),
        };
        let manifest_path = out_dir.join("manifest.json");
        write_json_atomic(&manifest_path, &manifest)?;
        std::fs::remove_file(&publishing_marker)?;
        return Ok(WikiOutcome {
            out_dir: out_dir.to_path_buf(),
            manifest_path,
            agent_index_path: out_dir.join("agent-index.json"),
            page_count: 0,
            community_count: graph.community_nodes.len(),
            route_count: graph.routes.len(),
            llm_enriched,
            pages_written: 0,
            pages_unchanged: 0,
        });
    }

    let mut sink = PageSink::new();

    let global_batch = emit_global_pages(&feature_groups, &graph, &input.repo_name, &mut sink)?;
    page_count += global_batch.pages.len();
    all_pages.extend(global_batch.pages);
    nav.extend(global_batch.nav);

    // All derived render state (globals + per-feature) computed once. Both the
    // batch loop below and the standalone `render_page` read from this.
    let ctx = RenderContext::build(&graph, &input, &feature_groups);

    // class_id → dev page slug (populated during dev page generation below).
    let mut class_dev_slugs: HashMap<String, String> = HashMap::new();
    // Agent-index collector: (class_node_id, source_file, relative_page_path).
    let mut dev_entries: Vec<(String, String, String)> = Vec::new();

    // Compute which features need re-rendering when --since is active.
    // Global and community pages are always re-rendered (fast, write-if-different handles mtimes).
    let affected_features: Option<std::collections::HashSet<String>> = input
        .changed_files
        .as_ref()
        .map(|changed| features_affected_by_changed_files(&feature_groups, &graph, changed));

    // Per-feature pages
    for group in &feature_groups {
        if let Some(ref af) = affected_features {
            if !af.contains(&group.feature) {
                continue;
            }
        }
        let batch = emit_feature_section(
            group,
            &graph,
            &ctx,
            out_dir,
            &mut class_dev_slugs,
            &mut sink,
            &mut dev_entries,
        )?;
        page_count += batch.pages.len();
        all_pages.extend(batch.pages);
        nav.extend(batch.nav);
    }

    // ── Scheduled jobs & event listeners ────────────────────────────────────
    {
        let ep_batch = emit_entrypoint_section(&graph, &ctx, out_dir, &class_dev_slugs, &mut sink)?;
        page_count += ep_batch.pages.len();
        all_pages.extend(ep_batch.pages);
        nav.extend(ep_batch.nav);
    }

    // ── Community pages ──────────────────────────────────────────────────────
    {
        let comm_batch = emit_community_section(&graph, &ctx, out_dir, &mut sink)?;
        page_count += comm_batch.pages.len();
        all_pages.extend(comm_batch.pages);
        nav.extend(comm_batch.nav);
    }

    // ── Partial render: merge unchanged features from previous manifest ───────
    if let Some(ref af) = affected_features {
        let manifest_path = out_dir.join("manifest.json");
        if let Ok(old_bytes) = std::fs::read(&manifest_path) {
            if let Ok(old) = serde_json::from_slice::<WikiManifest>(&old_bytes) {
                for page in old.pages {
                    let is_feature_page =
                        !matches!(page.role.as_str(), "system" | "shared" | "communities");
                    if is_feature_page && !af.contains(&page.role) {
                        page_count += 1;
                        all_pages.push(page);
                    }
                }
                for (feat, navs) in old.nav {
                    if !af.contains(&feat) {
                        nav.entry(feat).or_insert(navs);
                    }
                }
            }
        }
    }

    let manifest = WikiManifest {
        schema_version: 1,
        generated_at: cih_core::now_rfc3339(),
        repo_name: input.repo_name,
        graph_version: input.graph_version,
        community_version: input.community_version,
        stats,
        roles: vec!["po".into(), "ba".into(), "dev".into(), "communities".into()],
        nav,
        pages: all_pages,
        llm: input.llm_info,
        generation: Some(input.generation.clone()),
        module_tree_path: Some("module_tree.json".into()),
        wiki_meta_path: Some("wiki_meta.json".into()),
        warnings: Vec::new(),
    };

    // Snapshot rendered paths before flush so we can prune stale dev files.
    let rendered_paths: std::collections::HashSet<String> =
        sink.path_set().into_iter().map(String::from).collect();
    let flush_stats = sink.flush(out_dir)?;

    prune_stale_dev_files(
        &feature_groups,
        &affected_features,
        out_dir,
        &rendered_paths,
    );

    let manifest_path = out_dir.join("manifest.json");
    write_json_atomic(&manifest_path, &manifest)?;
    if input.generation.html_viewer {
        html::write_html_viewer(out_dir, &manifest)?;
    }

    // ── Agent index ──────────────────────────────────────────────────────────
    // Emit agent-index.json: two lookup maps for coding agents.
    //   fqn_to_page   — class node-id  → dev page path (relative to wiki out_dir)
    //   file_to_pages — source file     → [dev page paths]
    let agent_index_path = out_dir.join("agent-index.json");
    write_agent_index(&dev_entries, out_dir, &agent_index_path)?;
    std::fs::remove_file(&publishing_marker)?;

    Ok(WikiOutcome {
        out_dir: out_dir.to_path_buf(),
        manifest_path,
        agent_index_path,
        page_count,
        community_count: graph.community_nodes.len(),
        route_count: graph.routes.len(),
        llm_enriched,
        pages_written: flush_stats.written,
        pages_unchanged: flush_stats.unchanged,
    })
}

fn write_json_atomic(path: &std::path::Path, value: &impl serde::Serialize) -> anyhow::Result<()> {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

/// Remove stale dev-class `.md`/`.json` files left over from a prior run with a
/// different community assignment. Only touches features rendered this pass
/// (those in `affected_features`, or all when `None`).
fn prune_stale_dev_files(
    feature_groups: &[FeatureGroup],
    affected_features: &Option<std::collections::HashSet<String>>,
    out_dir: &Path,
    rendered_paths: &std::collections::HashSet<String>,
) {
    for group in feature_groups {
        if let Some(af) = affected_features {
            if !af.contains(&group.feature) {
                continue;
            }
        }
        let dev_dir = out_dir.join(format!("pages/{}/dev", group.feature));
        if dev_dir.exists() {
            for entry in std::fs::read_dir(&dev_dir).into_iter().flatten().flatten() {
                let path = entry.path();
                let is_page = path
                    .extension()
                    .map(|e| e == "md" || e == "json")
                    .unwrap_or(false);
                if !is_page {
                    continue;
                }
                // Build the relative path as pushed to the sink.
                if let Ok(rel) = path.strip_prefix(out_dir) {
                    let rel_str = rel.to_string_lossy().replace('\\', "/");
                    if !rendered_paths.contains(rel_str.as_str()) {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
    }
}

/// Emit `agent-index.json` at `agent_index_path`: `fqn_to_page` (class node-id →
/// dev page path) and `file_to_pages` (source file → dev page paths), the two
/// lookup maps coding agents use to jump from code to the generated wiki.
fn write_agent_index(
    dev_entries: &[(String, String, String)],
    out_dir: &Path,
    agent_index_path: &Path,
) -> Result<()> {
    let mut fqn_to_page: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let mut file_to_pages: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (class_id, file, page_path) in dev_entries {
        fqn_to_page.insert(class_id.clone(), page_path.clone());
        file_to_pages
            .entry(file.clone())
            .or_default()
            .push(page_path.clone());
    }
    let index_json = serde_json::to_string_pretty(&serde_json::json!({
        "schema_version": 1,
        "wiki_dir": out_dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("wiki"),
        "fqn_to_page": fqn_to_page,
        "file_to_pages": file_to_pages,
    }))?;
    std::fs::write(agent_index_path, index_json)?;
    Ok(())
}

pub(crate) fn capitalize(s: &str) -> String {
    let mut out = s.to_string();
    if let Some(first) = out.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    out
}

/// Returns true iff `s` is safe to use as a single filesystem path segment in the wiki
/// output directory. Only allows characters produced by `slugify` ([a-z0-9-]).
fn is_safe_page_slug(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn count_unresolved_refs(report: Option<&str>) -> usize {
    report
        .and_then(|r| {
            r.lines().find(|l| l.contains("**Total:**")).and_then(|l| {
                l.split("**Total:**")
                    .nth(1)
                    .and_then(|s| s.split('|').next())
                    .and_then(|s| s.trim().parse::<usize>().ok())
            })
        })
        .unwrap_or(0)
}

/// Map a set of changed file paths (relative to repo root) to the feature names that contain
/// nodes from those files. Used by the `--since` partial-render path.
fn features_affected_by_changed_files(
    feature_groups: &[FeatureGroup],
    graph: &WikiGraph,
    changed_files: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    let mut affected = std::collections::HashSet::new();
    for group in feature_groups {
        // Check community member nodes
        let has_changed_member = group.community_ids.iter().any(|cid| {
            graph
                .members_by_community
                .get(cid)
                .map(|members| members.iter().any(|m| changed_files.contains(&m.file)))
                .unwrap_or(false)
        });
        if has_changed_member {
            affected.insert(group.feature.clone());
            continue;
        }
        // For features driven by controller routes (including synthesized groups with no
        // community_ids), check route handler files.
        let has_changed_route = graph
            .routes_by_controller
            .iter()
            .filter(|(ctrl, _)| {
                graph
                    .controller_feature
                    .get(*ctrl)
                    .map(|f| f == &group.feature)
                    .unwrap_or(false)
            })
            .any(|(_, routes)| routes.iter().any(|(h, _)| changed_files.contains(&h.file)));
        if has_changed_route {
            affected.insert(group.feature.clone());
        }
    }
    affected
}

fn count_test_classes(graph: &WikiGraph) -> usize {
    graph
        .nodes_by_id
        .values()
        .filter(|n| {
            matches!(n.kind, NodeKind::Class | NodeKind::Interface)
                && node_stereotype(n) == Some("test")
        })
        .count()
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
