pub mod features;
pub mod graph;
pub mod html;
pub mod manifest;
pub mod mermaid;
pub mod module_tree;
pub mod pages;
pub mod slugify;

pub use cih_core::RepoMap;
pub use features::FeatureGroup;
pub use graph::WikiGraph;
pub use manifest::{
    NavEntry, PageEntry, WikiGenerationInfo, WikiLlmInfo, WikiManifest, WikiStats,
};
pub use module_tree::{
    build_graph_module_tree, build_wiki_meta, read_module_tree, validate_module_tree,
    ModuleTreeSource, WikiMeta, WikiModuleCacheEntry, WikiModuleNode, WikiModuleTree,
};

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Result;
use cih_core::{Node, NodeKind};
use features::{build_dev_page_paths, group_communities_by_feature};
use graph::node_stereotype;
use slugify::slugify;

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
    /// Only generate pages for features whose name contains one of these substrings
    /// (case-insensitive). Empty = no filter.
    pub filter_feature: Vec<String>,
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

pub fn generate_wiki(input: WikiInput<'_>, out_dir: &Path) -> Result<WikiOutcome> {
    let graph = WikiGraph::build(
        input.nodes,
        input.edges,
        input.community_nodes,
        input.community_edges,
    );

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
    let mut feature_groups = group_communities_by_feature(&graph);

    // No communities (discover not run): synthesize one group per feature found in
    // controller file paths so controller pages still get generated.
    if feature_groups.is_empty() && !graph.routes_by_controller.is_empty() {
        let mut features: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for feat in graph.controller_feature.values() {
            features.insert(feat.clone());
        }
        feature_groups = features
            .into_iter()
            .map(|feature| FeatureGroup { feature, community_ids: vec![] })
            .collect();
    }

    // Apply --filter-feature: keep only groups whose name contains a filter substring.
    if !input.filter_feature.is_empty() {
        let filters: Vec<String> = input.filter_feature.iter().map(|f| f.to_lowercase()).collect();
        feature_groups.retain(|g| {
            let name = g.feature.to_lowercase();
            filters.iter().any(|f| name.contains(f.as_str()))
        });
    }

    let dev_paths = build_dev_page_paths(&feature_groups, &graph);

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
        std::fs::create_dir_all(out_dir.join(format!("pages/{}/dev", group.feature)))?;
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
        let po_md = pages::feature_po::render_feature_po(
            feature,
            cids,
            &graph,
            input.llm_summaries.as_ref(),
            input.llm_full.as_ref(),
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

        // Per-community dev pages
        for comm_id in cids {
            let comm = match graph.nodes_by_id.get(comm_id) {
                Some(n) => n.clone(),
                None => continue,
            };
            let page_path = dev_paths
                .get(comm_id)
                .map(|s| s.as_str())
                .unwrap_or("shared/dev/community");
            let llm = input.llm_summaries.as_ref().and_then(|m| m.get(comm_id));
            let llm_full = input.llm_full.as_ref().and_then(|m| m.get(comm_id));
            let md = pages::dev::render_dev_community(&graph, &comm, page_path, llm, llm_full);
            let json_val = pages::dev::render_dev_community_json(&graph, &comm);
            std::fs::write(out_dir.join(format!("pages/{}.md", page_path)), &md)?;
            std::fs::write(
                out_dir.join(format!("pages/{}.json", page_path)),
                serde_json::to_string_pretty(&json_val)?,
            )?;
            let dev_title = page_path
                .split('/')
                .last()
                .map(|s| s.split('-').map(|w| {
                    let mut c = w.chars();
                    match c.next() {
                        None => String::new(),
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    }
                }).collect::<Vec<_>>().join(" "))
                .unwrap_or_else(|| comm.name.clone());
            nav.entry(feature.clone()).or_default().push(NavEntry {
                slug: page_path.to_string(),
                title: dev_title.clone(),
                kind: "dev".into(),
            });
            all_pages.push(PageEntry {
                slug: page_path.to_string(),
                role: feature.clone(),
                title: dev_title.clone(),
                kind: "dev".into(),
                path: format!("pages/{}.md", page_path),
                json_path: Some(format!("pages/{}.json", page_path)),
                community_id: Some(comm_id.clone()),
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
                // Apply LLM feature override only when file-path heuristic gives "shared"
                let effective_feature = if graph_feature == "shared" {
                    input.controller_summaries.as_ref()
                        .and_then(|m| m.get(*ctrl))
                        .and_then(|s| s.feature.as_deref())
                        .unwrap_or("shared")
                } else {
                    graph_feature
                };
                effective_feature == feature.as_str()
            })
            .map(|(ctrl, routes)| (ctrl.as_str(), routes))
            .collect();
        feature_controllers.sort_by_key(|(ctrl, _)| *ctrl);

        if !feature_controllers.is_empty() {
            std::fs::create_dir_all(out_dir.join(format!("pages/{}/controllers", feature)))?;
            for (ctrl_name, routes) in &feature_controllers {
                let slug = slugify(ctrl_name);
                let description = input.controller_summaries.as_ref()
                    .and_then(|m| m.get(*ctrl_name))
                    .map(|s| s.description.as_str())
                    .filter(|s| !s.is_empty());
                let ctrl_md = pages::feature_po::render_controller_page(ctrl_name, routes, description);
                let page_path = format!("{}/controllers/{}", feature, slug);
                std::fs::write(out_dir.join(format!("pages/{}.md", page_path)), &ctrl_md)?;
                nav.entry(feature.clone()).or_default().push(NavEntry {
                    slug: page_path.clone(),
                    title: ctrl_name.to_string(),
                    kind: "controller".into(),
                });
                all_pages.push(PageEntry {
                    slug: page_path.clone(),
                    role: feature.clone(),
                    title: ctrl_name.to_string(),
                    kind: "controller".into(),
                    path: format!("pages/{}.md", page_path),
                    json_path: None,
                    community_id: None,
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
        roles: vec!["po".into(), "ba".into(), "dev".into()],
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
mod tests {
    use super::*;
    use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn tmp_dir(label: &str) -> PathBuf {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "cih-wiki-test-{}-{}-{}",
            label,
            std::process::id(),
            id
        ))
    }

    fn make_node(id: &str, kind: NodeKind, name: &str) -> Node {
        Node {
            id: NodeId::new(id.to_string()),
            kind,
            name: name.to_string(),
            qualified_name: None,
            file: "Test.java".to_string(),
            range: Range::default(),
            props: None,
        }
    }

    fn minimal_input<'a>(
        nodes: &'a [Node],
        edges: &'a [Edge],
        comm_nodes: &'a [Node],
        comm_edges: &'a [Edge],
    ) -> WikiInput<'a> {
        WikiInput {
            nodes,
            edges,
            community_nodes: comm_nodes,
            community_edges: comm_edges,
            repo_name: "test-service".to_string(),
            graph_version: "abc123".to_string(),
            community_version: "def456".to_string(),
            unresolved_report: None,
            repo_map: None,
            llm_summaries: None,
            llm_full: None,
            llm_info: None,
            module_tree: None,
            generation: WikiGenerationInfo::default(),
            first_module_tree: None,
            save_evidence: None,
            controller_summaries: None,
            filter_feature: vec![],
        }
    }

    #[test]
    fn generate_wiki_writes_expected_files() {
        // Method:com.example.Foo#bar/0 in Test.java → feature=shared, class=Foo, slug=foo
        let sym = make_node("Method:com.example.Foo#bar/0", NodeKind::Method, "bar");
        let comm = make_node("Community:0", NodeKind::Community, "order-service");
        let comm_edges = [Edge {
            src: sym.id.clone(),
            dst: NodeId::new("Community:0".to_string()),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
        }];
        let nodes = [sym];
        let comm_nodes = [comm];

        let out = tmp_dir("expected-files");
        let input = minimal_input(&nodes, &[], &comm_nodes, &comm_edges);
        let outcome = generate_wiki(input, &out).unwrap();

        assert!(out.join("manifest.json").exists(), "manifest.json");
        assert!(out.join("module_tree.json").exists(), "module_tree.json");
        assert!(out.join("wiki_meta.json").exists(), "wiki_meta.json");
        assert!(out.join("pages/index.md").exists(), "system index");
        assert!(out.join("pages/routes.md").exists(), "routes.md");
        // Feature-first paths: Test.java has no modules/ → feature=shared
        assert!(
            out.join("pages/shared/index.md").exists(),
            "shared/index.md"
        );
        assert!(out.join("pages/shared/po.md").exists(), "shared/po.md");
        assert!(out.join("pages/shared/ba.md").exists(), "shared/ba.md");
        // class Foo → slug=foo
        assert!(
            out.join("pages/shared/dev/foo.md").exists(),
            "shared/dev/foo.md"
        );
        assert_eq!(outcome.community_count, 1);

        let manifest_json = std::fs::read_to_string(out.join("manifest.json")).unwrap();
        let manifest: WikiManifest = serde_json::from_str(&manifest_json).unwrap();
        assert_eq!(manifest.schema_version, 1);
        assert_eq!(manifest.repo_name, "test-service");
        assert!(manifest.llm.is_none(), "llm absent when not enriched");

        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn generate_wiki_records_llm_model_in_manifest_when_enriched() {
        // Method:com.example.Bar#run/0 in Test.java → feature=shared, class=Bar, slug=bar
        let sym = make_node("Method:com.example.Bar#run/0", NodeKind::Method, "run");
        let comm = make_node("Community:1", NodeKind::Community, "payment-service");
        let comm_edges = [Edge {
            src: sym.id.clone(),
            dst: NodeId::new("Community:1".to_string()),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
        }];

        let mut summaries = HashMap::new();
        summaries.insert(
            "Community:1".to_string(),
            CommunityLlmSummary {
                po: "Handles payments.".to_string(),
                ba: "Processes payment flows.".to_string(),
                dev: "Uses service-repository.".to_string(),
            },
        );

        let out = tmp_dir("llm-manifest");
        let input = WikiInput {
            nodes: &[sym],
            edges: &[],
            community_nodes: &[comm],
            community_edges: &comm_edges,
            repo_name: "payment".to_string(),
            graph_version: "v1".to_string(),
            community_version: "v2".to_string(),
            unresolved_report: None,
            repo_map: None,
            llm_summaries: Some(summaries),
            llm_full: None,
            llm_info: Some(WikiLlmInfo {
                provider: "anthropic".to_string(),
                model: "claude-haiku-4-5-20251001".to_string(),
                language: "en".to_string(),
                evidence_file_count: 1,
                enriched_community_count: 1,
                failed_community_count: 0,
                failed_community_ids: Vec::new(),
            }),
            module_tree: None,
            generation: WikiGenerationInfo::default(),
            first_module_tree: None,
            save_evidence: None,
            controller_summaries: None,
            filter_feature: vec![],
        };
        let outcome = generate_wiki(input, &out).unwrap();

        let manifest_json = std::fs::read_to_string(out.join("manifest.json")).unwrap();
        let manifest: WikiManifest = serde_json::from_str(&manifest_json).unwrap();
        let llm = manifest.llm.as_ref().expect("llm metadata");
        assert_eq!(Some(llm.model.as_str()), Some("claude-haiku-4-5-20251001"));
        assert_eq!(llm.provider, "anthropic");
        assert!(outcome.llm_enriched);

        // PO page for the shared feature includes LLM summary
        let po_page = std::fs::read_to_string(out.join("pages/shared/po.md")).unwrap();
        assert!(po_page.contains("## Overview"), "po page has overview");
        assert!(po_page.contains("Handles payments"), "po page has llm text");

        let _ = std::fs::remove_dir_all(&out);
    }
}
