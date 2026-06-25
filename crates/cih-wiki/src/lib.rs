pub mod bodies;
pub mod features;
pub mod graph;
pub mod html;
pub mod manifest;
pub mod mermaid;
pub mod module_tree;
pub mod pages;
pub mod slugify;

pub use bodies::{source_bodies, BodyEntry};
pub use cih_core::RepoMap;
pub use features::{assign_class_slugs, FeatureGroup};
pub use graph::WikiGraph;
pub use manifest::{NavEntry, PageEntry, WikiGenerationInfo, WikiLlmInfo, WikiManifest, WikiStats};
pub use module_tree::{
    build_graph_module_tree, build_wiki_meta, read_module_tree, validate_module_tree,
    ClassCacheEntry, ClassEnrichmentStore, FeatureMetaEntry, ModuleTreeSource, WikiMeta,
    WikiModuleCacheEntry, WikiModuleNode, WikiModuleTree,
};

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Result;
use cih_core::{Node, NodeId, NodeKind, Range};
use features::{build_dev_page_paths, group_communities_by_feature, group_nodes_by_package};
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
#[derive(Clone, Debug, Default)]
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
    pub bodies: HashMap<String, BodyEntry>,
    /// Maps `(node_id, file)` to a feature slug. Supplied by `cih-engine`; called during
    /// `WikiGraph::build_package_grouped`. When grouping is "graph"/"llm" never called.
    /// When a pre-computed artifact is available, `node_id` gives a direct lookup;
    /// otherwise fall back to file-path heuristics.
    pub feature_of: Box<dyn Fn(&str, &str) -> String + Send>,
    /// Scheduled jobs and event listeners from `.cih/entrypoints.json`.
    /// Empty when the sidecar does not exist (no such methods in the repo).
    pub entrypoints: Vec<EntrypointRecord>,
}

#[derive(Debug)]
pub struct WikiOutcome {
    pub out_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub page_count: usize,
    pub community_count: usize,
    pub route_count: usize,
    pub llm_enriched: bool,
}

/// Strip redundant class/method references that the LLM sometimes prepends to descriptions.
/// Handles patterns like `ClassName.methodName/N()`, "The ClassName ...", `` `methodName` ``.
fn clean_method_desc(desc: &str, cls: &str, meth: &str) -> String {
    let mut s = desc.trim().to_string();

    // Strip ClassName.methodName/N() or ClassName.methodName/N() style prefix (with optional backticks)
    // e.g. "`DelinquencyApiResource`.updateDelinquencyBucket/2() processes..."
    // e.g. "DelinquencyApiResource.updateDelinquencyBucket/2() processes..."
    let prefix_dot = format!("{}.", cls);
    let prefix_bt = format!("`{}`.", cls);
    for prefix in &[prefix_bt.as_str(), prefix_dot.as_str()] {
        if let Some(rest) = s.strip_prefix(prefix) {
            // consume methodName/N() or just methodName
            let after = rest
                .find(|c: char| c == ' ' || c == '\n')
                .map(|i| rest[i..].trim_start())
                .unwrap_or("");
            if !after.is_empty() {
                s = after.to_string();
                break;
            }
        }
    }

    // Strip "The ClassName " at start (e.g. "The DelinquencyApiResource resource handles...")
    let the_cls = format!("The {} ", cls);
    if let Some(rest) = s.strip_prefix(&the_cls) {
        // Lowercase first char since we removed the "The ClassName " subject
        // Only do this if the remainder looks like a predicate (starts lowercase or verb)
        s = rest.to_string();
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

pub fn generate_wiki(input: WikiInput<'_>, out_dir: &Path) -> Result<WikiOutcome> {
    let graph = if input.grouping == "package" {
        WikiGraph::build_package_grouped(input.nodes, input.edges, &*input.feature_of)
    } else {
        WikiGraph::build(input.nodes, input.edges, input.community_nodes, input.community_edges)
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

    // Feature grouping — the core of the new hierarchy
    let mut feature_groups = if input.grouping == "package" {
        // Restrict to packages that survived --filter-route (stored in input.community_nodes).
        // When no route filter was active, input.community_nodes contains all packages.
        let allowed_ids: std::collections::HashSet<&str> =
            input.community_nodes.iter().map(|n| n.id.as_str()).collect();
        let all_groups = group_nodes_by_package(&graph);
        if allowed_ids.is_empty() {
            all_groups
        } else {
            all_groups
                .into_iter()
                .filter(|g| g.community_ids.iter().any(|id| allowed_ids.contains(id.as_str())))
                .collect()
        }
    } else {
        group_communities_by_feature(&graph)
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

    let dev_paths = build_dev_page_paths(&feature_groups, &graph);

    // Pre-collect known feature names so the controller filter below can validate
    // LLM-suggested feature slugs — prevents controllers from being silently dropped
    // when DeepSeek/Gemini returns a slug that doesn't match any real wiki feature.
    let known_features: std::collections::HashSet<String> =
        feature_groups.iter().map(|g| g.feature.clone()).collect();

    // Flat method_id → description lookup built once from flow_llm_summaries.
    let mut method_flow_desc: HashMap<String, String> = {
        let mut map = HashMap::new();
        if let Some(flow_map) = &input.flow_llm_summaries {
            for (proc_id, summary) in flow_map {
                if let Some(steps) = graph.process_steps.get(proc_id.as_str()) {
                    for step in steps {
                        let idx = (step.step_number as usize).saturating_sub(1);
                        if let Some(desc) = summary.step_descriptions.get(idx) {
                            if !desc.is_empty() {
                                map.insert(step.symbol.id.as_str().to_string(), desc.clone());
                            }
                        }
                    }
                }
            }
        }
        map
    };

    // Merge per-method descriptions from controller LLM enrichment into method_flow_desc.
    // The controller summaries use simple Java method names as keys; resolve them to full
    // node IDs by scanning all methods in the graph.
    if let Some(ctrl_summaries) = &input.controller_summaries {
        for method_nodes in graph.methods_by_class.values() {
            for method in method_nodes {
                let method_id = method.id.as_str();
                let class_name = method_id
                    .split_once('#')
                    .and_then(|(prefix, _)| {
                        prefix
                            .trim_start_matches("Method:")
                            .trim_start_matches("Constructor:")
                            .rsplit('.')
                            .next()
                    });
                let simple_method_name = method_id
                    .split('#')
                    .nth(1)
                    .and_then(|x| x.split('/').next());
                if let (Some(cls), Some(meth)) = (class_name, simple_method_name) {
                    if let Some(ctrl_summary) = ctrl_summaries.get(cls) {
                        if let Some(desc) = ctrl_summary.method_descriptions.get(meth) {
                            method_flow_desc
                                .entry(method_id.to_string())
                                .or_insert_with(|| clean_method_desc(desc, cls, meth));
                        }
                    }
                }
            }
        }
    }

    let module_tree = input.module_tree.unwrap_or_else(|| {
        build_graph_module_tree(
            &graph,
            input.repo_map.as_ref(),
            &input.graph_version,
            &input.community_version,
            None,
        )
    });
    validate_module_tree(&module_tree, &graph)?;
    let wiki_meta = build_wiki_meta(
        &module_tree,
        input.llm_info.as_ref().map(|info| info.model.clone()),
        input.llm_info.as_ref().map(|info| info.language.clone()),
    );

    // Create directories
    std::fs::create_dir_all(out_dir)?;
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
        // Remove stale .md/.json files left over from a prior community-based run
        if dev_dir.exists() {
            for entry in std::fs::read_dir(&dev_dir)? {
                let path = entry?.path();
                if path
                    .extension()
                    .map(|e| e == "md" || e == "json")
                    .unwrap_or(false)
                {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
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
        std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;
        return Ok(WikiOutcome {
            out_dir: out_dir.to_path_buf(),
            manifest_path,
            page_count: 0,
            community_count: graph.community_nodes.len(),
            route_count: graph.routes.len(),
            llm_enriched,
        });
    }

    // System index
    let system_md =
        pages::system_index::render_system_index(&feature_groups, &graph, &input.repo_name);
    std::fs::write(out_dir.join("pages/index.md"), &system_md)?;
    all_pages.push(PageEntry {
        slug: "index".into(),
        role: "system".into(),
        title: input.repo_name.clone(),
        kind: "index".into(),
        path: "pages/index.md".into(),
        json_path: None,
        community_id: None,
    });
    page_count += 1;

    // Shared routes (global aggregation)
    let routes_md = pages::shared::render_routes_page(&graph);
    let routes_json = pages::shared::render_routes_json(&graph);
    std::fs::write(out_dir.join("pages/routes.md"), &routes_md)?;
    std::fs::write(
        out_dir.join("pages/routes.json"),
        serde_json::to_string_pretty(&routes_json)?,
    )?;
    all_pages.push(PageEntry {
        slug: "routes".into(),
        role: "shared".into(),
        title: "API Routes".into(),
        kind: "routes".into(),
        path: "pages/routes.md".into(),
        json_path: Some("pages/routes.json".into()),
        community_id: None,
    });
    page_count += 1;

    // Pre-pass: for each class, determine its primary feature (the one with the most methods).
    // This prevents a class from appearing as a page in multiple features when its methods are
    // spread across communities that belong to different features (e.g. CouponController having
    // a cart-calling method landing it in the cart community).
    let class_primary_feature: std::collections::HashMap<String, String> = {
        let mut votes: std::collections::HashMap<String, std::collections::BTreeMap<String, usize>> =
            std::collections::HashMap::new();
        for group in &feature_groups {
            for comm_id in &group.community_ids {
                if let Some(members) = graph.members_by_community.get(comm_id) {
                    for m in members {
                        if !matches!(
                            m.kind,
                            NodeKind::Method | NodeKind::Function | NodeKind::Constructor
                        ) {
                            continue;
                        }
                        if let Some(cls_id) =
                            m.id.as_str().split_once('#').map(|(prefix, _)| {
                                let fqcn = prefix
                                    .trim_start_matches("Method:")
                                    .trim_start_matches("Constructor:")
                                    .trim_start_matches("Function:");
                                ["Class:", "Interface:", "Enum:", "Record:"]
                                    .iter()
                                    .map(|pfx| format!("{}{}", pfx, fqcn))
                                    .find(|id| {
                                        graph.nodes_by_id.contains_key(id.as_str())
                                            || graph.methods_by_class.contains_key(id.as_str())
                                    })
                                    .unwrap_or_else(|| format!("Class:{}", fqcn))
                            })
                        {
                            *votes
                                .entry(cls_id)
                                .or_default()
                                .entry(group.feature.clone())
                                .or_insert(0) += 1;
                        }
                    }
                }
            }
        }
        votes
            .into_iter()
            .map(|(cls_id, feature_votes)| {
                let best = feature_votes
                    .into_iter()
                    .max_by_key(|(_, v)| *v)
                    .map(|(f, _)| f)
                    .unwrap_or_default();
                (cls_id, best)
            })
            .collect()
    };

    // Build handler_id → process_id lookup (used for flow pages).
    let process_by_handler: HashMap<String, String> = graph
        .process_steps
        .iter()
        .filter_map(|(proc_id, steps)| {
            steps
                .first()
                .map(|s| (s.symbol.id.as_str().to_string(), proc_id.clone()))
        })
        .collect();

    // class_id → dev page slug (populated during dev page generation below).
    let mut class_dev_slugs: HashMap<String, String> = HashMap::new();

    // Pre-compute per-feature entrypoint counts for the PO page API Surface table.
    let mut feature_scheduled_counts: HashMap<String, usize> = HashMap::new();
    let mut feature_listener_counts: HashMap<String, usize> = HashMap::new();
    for ep in &input.entrypoints {
        let file = graph
            .nodes_by_id
            .get(ep.method_id.as_str())
            .map(|n| n.file.as_str())
            .unwrap_or("");
        let feature = (input.feature_of)(ep.method_id.as_str(), file);
        match ep.kind.as_str() {
            "scheduled" => *feature_scheduled_counts.entry(feature).or_insert(0) += 1,
            "event_listener" => *feature_listener_counts.entry(feature).or_insert(0) += 1,
            _ => {}
        }
    }

    // Per-feature pages
    for group in &feature_groups {
        let feature = &group.feature;
        let cids = &group.community_ids;

        // Feature landing index
        let idx_md = pages::feature_index::render_feature_index(feature, cids, &dev_paths, &graph);
        std::fs::write(out_dir.join(format!("pages/{}/index.md", feature)), &idx_md)?;
        nav.entry(feature.clone()).or_default().push(NavEntry {
            slug: format!("{}/index", feature),
            title: format!("{} Overview", capitalize(feature)),
            kind: "index".into(),
        });
        all_pages.push(PageEntry {
            slug: format!("{}/index", feature),
            role: feature.clone(),
            title: format!("{} Overview", capitalize(feature)),
            kind: "index".into(),
            path: format!("pages/{}/index.md", feature),
            json_path: None,
            community_id: None,
        });
        page_count += 1;

        // Feature PO
        let feature_llm = input
            .feature_llm_summaries
            .as_ref()
            .and_then(|m| m.get(feature.as_str()));
        let po_md = pages::feature_po::render_feature_po(
            feature,
            cids,
            &graph,
            input.llm_summaries.as_ref(),
            input.llm_full.as_ref(),
            feature_llm,
            input.flow_llm_summaries.as_ref(),
            feature_scheduled_counts.get(feature.as_str()).copied().unwrap_or(0),
            feature_listener_counts.get(feature.as_str()).copied().unwrap_or(0),
        );
        std::fs::write(out_dir.join(format!("pages/{}/po.md", feature)), &po_md)?;
        nav.entry(feature.clone()).or_default().push(NavEntry {
            slug: format!("{}/po", feature),
            title: format!("{} — Business Overview", capitalize(feature)),
            kind: "po".into(),
        });
        all_pages.push(PageEntry {
            slug: format!("{}/po", feature),
            role: feature.clone(),
            title: format!("{} — Business Overview", capitalize(feature)),
            kind: "po".into(),
            path: format!("pages/{}/po.md", feature),
            json_path: None,
            community_id: None,
        });
        page_count += 1;

        // Feature BA
        let ba_md = pages::feature_ba::render_feature_ba(
            feature,
            cids,
            &graph,
            input.llm_summaries.as_ref(),
            input.llm_full.as_ref(),
            feature_llm,
            input.flow_llm_summaries.as_ref(),
        );
        std::fs::write(out_dir.join(format!("pages/{}/ba.md", feature)), &ba_md)?;
        nav.entry(feature.clone()).or_default().push(NavEntry {
            slug: format!("{}/ba", feature),
            title: format!("{} — Business Analysis", capitalize(feature)),
            kind: "ba".into(),
        });
        all_pages.push(PageEntry {
            slug: format!("{}/ba", feature),
            role: feature.clone(),
            title: format!("{} — Business Analysis", capitalize(feature)),
            kind: "ba".into(),
            path: format!("pages/{}/ba.md", feature),
            json_path: None,
            community_id: None,
        });
        page_count += 1;

        // Per-class dev pages (one page per class, replacing per-community pages)
        // Collect distinct classes that have methods in any community of this feature.
        let mut feature_class_set: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for comm_id in cids {
            if let Some(members) = graph.members_by_community.get(comm_id) {
                for m in members {
                    if !matches!(
                        m.kind,
                        NodeKind::Method | NodeKind::Function | NodeKind::Constructor
                    ) {
                        continue;
                    }
                    if let Some(cls_id) =
                        m.id.as_str().split_once('#').map(|(prefix, _)| {
                            let fqcn = prefix
                                .trim_start_matches("Method:")
                                .trim_start_matches("Constructor:")
                                .trim_start_matches("Function:");
                            // The class node may be Class:, Interface:, Enum:, or Record: —
                            // resolve against the actual graph rather than always guessing Class:.
                            ["Class:", "Interface:", "Enum:", "Record:"]
                                .iter()
                                .map(|pfx| format!("{}{}", pfx, fqcn))
                                .find(|id| {
                                    graph.nodes_by_id.contains_key(id.as_str())
                                        || graph.methods_by_class.contains_key(id.as_str())
                                })
                                .unwrap_or_else(|| format!("Class:{}", fqcn))
                        })
                    {
                        // Only include this class if this feature is its primary owner.
                        if class_primary_feature
                            .get(&cls_id)
                            .map(|f| f == feature)
                            .unwrap_or(true)
                        {
                            feature_class_set.insert(cls_id);
                        }
                    }
                }
            }
        }

        // Assign dev-page slugs using the shared two-pass collision counter.
        let slug_for = features::assign_class_slugs(&feature_class_set, |id| {
            graph
                .nodes_by_id
                .get(id)
                .map(|n| n.name.clone())
                .unwrap_or_else(|| {
                    id.trim_start_matches("Class:")
                        .rsplit('.')
                        .next()
                        .unwrap_or("Unknown")
                        .to_string()
                })
        });

        for class_id in &feature_class_set {
            let slug = slug_for
                .get(class_id.as_str())
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            let page_path = format!("{}/dev/{}", feature, slug);
            // Capture slug for use in flow page Technical Reference links.
            class_dev_slugs.insert(class_id.clone(), slug.clone());

            // Build or synthesize the class node (class nodes may be absent when only
            // method nodes are present in the raw graph — synthesize a stub in that case)
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
            let md = pages::dev::render_dev_class(&graph, cls_node, &input.bodies, &method_flow_desc);
            let json_val = pages::dev::render_dev_class_json(&graph, cls_node);
            std::fs::write(out_dir.join(format!("pages/{}.md", page_path)), &md)?;
            std::fs::write(
                out_dir.join(format!("pages/{}.json", page_path)),
                serde_json::to_string_pretty(&json_val)?,
            )?;
            let dev_title = cls_node.name.clone();
            nav.entry(feature.clone()).or_default().push(NavEntry {
                slug: page_path.clone(),
                title: dev_title.clone(),
                kind: "dev".into(),
            });
            all_pages.push(PageEntry {
                slug: page_path.clone(),
                role: feature.clone(),
                title: dev_title,
                kind: "dev".into(),
                path: format!("pages/{}.md", page_path),
                json_path: Some(format!("pages/{}.json", page_path)),
                community_id: None,
            });
            page_count += 1;
        }

        // Controller pages for this feature
        let mut feature_controllers: Vec<(&str, &Vec<(Node, Node)>)> = graph
            .routes_by_controller
            .iter()
            .filter(|(ctrl, _)| {
                let graph_feature = graph
                    .controller_feature
                    .get(*ctrl)
                    .map(|f| f.as_str())
                    .unwrap_or("shared");
                // Apply LLM feature override only when file-path heuristic gives "shared",
                // AND only when the LLM-suggested slug actually exists as a wiki feature.
                // Without the existence check, LLMs often return domain-specific slugs
                // ("delinquency", "rescheduling") that don't match any feature group,
                // silently dropping all route flow pages for the module.
                let effective_feature = if graph_feature == "shared" {
                    let llm_feat = input
                        .controller_summaries
                        .as_ref()
                        .and_then(|m| m.get(*ctrl))
                        .and_then(|s| s.feature.as_deref())
                        .unwrap_or("shared");
                    if known_features.contains(llm_feat) {
                        llm_feat
                    } else {
                        graph_feature
                    }
                } else {
                    graph_feature
                };
                effective_feature == feature.as_str()
            })
            .map(|(ctrl, routes)| (ctrl.as_str(), routes))
            .collect();
        feature_controllers.sort_by_key(|(ctrl, _)| *ctrl);

        if !feature_controllers.is_empty() {
            let api_dir = out_dir.join(format!("pages/{}/api", feature));
            std::fs::create_dir_all(&api_dir)?;
            std::fs::write(
                api_dir.join("_category_.json"),
                "{\"position\": 3, \"label\": \"API Surface\"}\n",
            )?;
            for (ctrl_pos, (ctrl_name, routes)) in feature_controllers.iter().enumerate() {
                let ctrl_slug = slugify(ctrl_name);
                let display_title = pages::feature_po::controller_display_name(ctrl_name);
                let ctrl_summary = input
                    .controller_summaries
                    .as_ref()
                    .and_then(|m| m.get(*ctrl_name));
                let description = ctrl_summary
                    .map(|s| s.description.as_str())
                    .filter(|s| !s.is_empty());
                let empty_methods = HashMap::new();
                let method_descriptions = ctrl_summary
                    .map(|s| &s.method_descriptions)
                    .unwrap_or(&empty_methods);

                // Remove any stale top-level controller .md files left from Phase 1.
                let stale = api_dir.join(format!("{}.md", ctrl_slug));
                let _ = std::fs::remove_file(&stale);

                // Subdirectory for this controller's flow pages.
                let ctrl_dir = api_dir.join(&ctrl_slug);
                std::fs::create_dir_all(&ctrl_dir)?;
                // Docusaurus 3.5+ auto-links index.md as the category page — no `link` needed.
                std::fs::write(
                    ctrl_dir.join("_category_.json"),
                    format!(
                        "{{\"position\": {}, \"label\": \"{}\", \"collapsible\": true, \"collapsed\": false}}\n",
                        ctrl_pos + 1,
                        display_title
                    ),
                )?;

                // index.md — controller overview (route table + descriptions).
                let ctrl_md = pages::feature_po::render_controller_page(
                    ctrl_name,
                    routes,
                    description,
                    method_descriptions,
                );
                std::fs::write(ctrl_dir.join("index.md"), &ctrl_md)?;

                // One flow page per route handler.
                for (route_pos, (handler, route)) in routes.iter().enumerate() {
                    let handler_slug = pages::api_flow::handler_slug(handler.id.as_str());
                    let process_id = process_by_handler.get(handler.id.as_str());
                    let flow_summary = process_id
                        .and_then(|pid| {
                            input.flow_llm_summaries.as_ref()?.get(pid.as_str())
                        })
                        .or_else(|| {
                            input.flow_llm_summaries.as_ref()?.get(handler.id.as_str())
                        });
                    let flow_md = pages::api_flow::render_api_flow_page(
                        handler,
                        route,
                        route_pos + 1,
                        flow_summary,
                        &graph,
                        &class_dev_slugs,
                        &method_flow_desc,
                    );
                    let page_path = format!("{}/api/{}/{}", feature, ctrl_slug, handler_slug);
                    std::fs::write(
                        out_dir.join(format!("pages/{}.md", page_path)),
                        &flow_md,
                    )?;
                    let flow_title = pages::api_flow::handler_title(handler.id.as_str());
                    nav.entry(feature.clone()).or_default().push(NavEntry {
                        slug: page_path.clone(),
                        title: flow_title.clone(),
                        kind: "api-flow".into(),
                    });
                    all_pages.push(PageEntry {
                        slug: page_path.clone(),
                        role: feature.clone(),
                        title: flow_title,
                        kind: "api-flow".into(),
                        path: format!("pages/{}.md", page_path),
                        json_path: None,
                        community_id: None,
                    });
                    page_count += 1;
                }
            }
        }
    }

    // ── Scheduled jobs & event listeners ────────────────────────────────────
    if !input.entrypoints.is_empty() {
        // Build a flat method-desc map from all controller LLM summaries.
        let all_method_desc: HashMap<String, String> = input
            .controller_summaries
            .iter()
            .flat_map(|m| m.values())
            .flat_map(|s| s.method_descriptions.iter())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // Group by feature
        let mut by_feature_scheduled: BTreeMap<String, Vec<&crate::EntrypointRecord>> =
            BTreeMap::new();
        let mut by_feature_events: BTreeMap<String, Vec<&crate::EntrypointRecord>> =
            BTreeMap::new();

        for ep in &input.entrypoints {
            let file = graph
                .nodes_by_id
                .get(ep.method_id.as_str())
                .map(|n| n.file.as_str())
                .unwrap_or("");
            let feature = (input.feature_of)(ep.method_id.as_str(), file);
            match ep.kind.as_str() {
                "scheduled" => by_feature_scheduled.entry(feature).or_default().push(ep),
                "event_listener" => by_feature_events.entry(feature).or_default().push(ep),
                _ => {}
            }
        }

        // Write pages for each feature's scheduled jobs
        for (feature, entries) in &by_feature_scheduled {
            let api_dir = out_dir.join(format!("pages/{}/api", feature));
            std::fs::create_dir_all(&api_dir)?;
            // Ensure api/_category_.json exists (may not if no HTTP controllers).
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
                    &graph,
                    &class_dev_slugs,
                    &all_method_desc,
                );
                let page_path = format!("{}/api/scheduled/{}", feature, slug);
                std::fs::write(
                    out_dir.join(format!("pages/{}.md", page_path)),
                    &md,
                )?;
                let flow_title = pages::api_flow::handler_title(ep.method_id.as_str());
                nav.entry(feature.clone()).or_default().push(NavEntry {
                    slug: page_path.clone(),
                    title: flow_title.clone(),
                    kind: "scheduled-flow".into(),
                });
                all_pages.push(PageEntry {
                    slug: page_path.clone(),
                    role: feature.clone(),
                    title: flow_title,
                    kind: "scheduled-flow".into(),
                    path: format!("pages/{}.md", page_path),
                    json_path: None,
                    community_id: None,
                });
                page_count += 1;
            }
        }

        // Write pages for each feature's event listeners
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
                    &graph,
                    &class_dev_slugs,
                    &all_method_desc,
                );
                let page_path = format!("{}/api/events/{}", feature, slug);
                std::fs::write(
                    out_dir.join(format!("pages/{}.md", page_path)),
                    &md,
                )?;
                let flow_title = pages::api_flow::handler_title(ep.method_id.as_str());
                nav.entry(feature.clone()).or_default().push(NavEntry {
                    slug: page_path.clone(),
                    title: flow_title.clone(),
                    kind: "listener-flow".into(),
                });
                all_pages.push(PageEntry {
                    slug: page_path.clone(),
                    role: feature.clone(),
                    title: flow_title,
                    kind: "listener-flow".into(),
                    path: format!("pages/{}.md", page_path),
                    json_path: None,
                    community_id: None,
                });
                page_count += 1;
            }
        }
    }

    // ── Community pages ──────────────────────────────────────────────────────
    {
        let comm_slug_map = slugify::build_slug_map(&graph.community_nodes);
        std::fs::create_dir_all(out_dir.join("pages/communities"))?;

        let comm_idx = pages::community::render_community_index(
            &graph.community_nodes,
            &comm_slug_map,
            &graph,
        );
        std::fs::write(out_dir.join("pages/communities/index.md"), &comm_idx)?;
        all_pages.push(PageEntry {
            slug: "communities/index".into(),
            role: "communities".into(),
            title: "Communities".into(),
            kind: "index".into(),
            path: "pages/communities/index.md".into(),
            json_path: None,
            community_id: None,
        });
        page_count += 1;

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

            let llm = input.llm_summaries.as_ref().and_then(|m| m.get(&comm_id));
            let llm_full = input.llm_full.as_ref().and_then(|m| m.get(&comm_id));

            let detail_md = pages::community::render_community_detail(
                comm,
                &graph,
                &processes_here,
                llm,
            );
            std::fs::write(dir.join("index.md"), &detail_md)?;
            all_pages.push(PageEntry {
                slug: format!("communities/{dir_name}/index"),
                role: "communities".into(),
                title: comm.name.clone(),
                kind: "index".into(),
                path: format!("pages/communities/{dir_name}/index.md"),
                json_path: None,
                community_id: Some(comm_id.clone()),
            });
            page_count += 1;

            if let Some(full) = llm_full {
                let po_md = pages::community::render_community_po(comm, &graph, full);
                std::fs::write(dir.join("po.md"), &po_md)?;
                all_pages.push(PageEntry {
                    slug: format!("communities/{dir_name}/po"),
                    role: "communities".into(),
                    title: format!("{} — Business Overview", comm.name),
                    kind: "po".into(),
                    path: format!("pages/communities/{dir_name}/po.md"),
                    json_path: None,
                    community_id: Some(comm_id.clone()),
                });
                page_count += 1;

                let ba_md = pages::community::render_community_ba(
                    comm,
                    &graph,
                    &processes_here,
                    full,
                );
                std::fs::write(dir.join("ba.md"), &ba_md)?;
                all_pages.push(PageEntry {
                    slug: format!("communities/{dir_name}/ba"),
                    role: "communities".into(),
                    title: format!("{} — Business Analysis", comm.name),
                    kind: "ba".into(),
                    path: format!("pages/communities/{dir_name}/ba.md"),
                    json_path: None,
                    community_id: Some(comm_id.clone()),
                });
                page_count += 1;
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

    let manifest_path = out_dir.join("manifest.json");
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;
    if input.generation.html_viewer {
        html::write_html_viewer(out_dir, &manifest)?;
    }

    Ok(WikiOutcome {
        out_dir: out_dir.to_path_buf(),
        manifest_path,
        page_count,
        community_count: graph.community_nodes.len(),
        route_count: graph.routes.len(),
        llm_enriched,
    })
}

fn capitalize(s: &str) -> String {
    let mut out = s.to_string();
    if let Some(first) = out.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    out
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
mod tests;


