pub mod graph;
pub mod manifest;
pub mod pages;
pub mod slugify;

pub use cih_core::RepoMap;
pub use graph::WikiGraph;
pub use manifest::{NavEntry, PageEntry, WikiManifest, WikiStats};

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Result;
use cih_core::{Node, NodeKind};
use graph::node_stereotype;
use slugify::build_slug_map;

/// Pre-computed AI summaries for one community (all three roles).
/// Produced by the engine's `enrich_communities()`; passed into `WikiInput`.
#[derive(Clone, Debug, Default)]
pub struct CommunityLlmSummary {
    /// 2-3 sentences in plain business language.
    pub po: String,
    /// 2-3 sentences on workflows, contracts, and events.
    pub ba: String,
    /// 2-3 sentences on technical structure.
    pub dev: String,
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
    /// Keyed by community_id (e.g. `"Community:3"`). `None` = graph-only mode.
    pub llm_summaries: Option<HashMap<String, CommunityLlmSummary>>,
    /// Model name used for enrichment, recorded in the manifest.
    pub llm_model: Option<String>,
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
    let slug_map = build_slug_map(&graph.community_nodes);

    let unresolved_count = count_unresolved_refs(input.unresolved_report.as_deref());
    let class_count: usize = graph.community_class_counts.values().sum();
    let test_class_count = count_test_classes(&graph);

    let stats = WikiStats {
        community_count: graph.community_nodes.len(),
        route_count: graph.routes.len(),
        process_count: graph.process_nodes.len(),
        class_count,
        test_class_count,
        unresolved_ref_count: unresolved_count,
    };

    std::fs::create_dir_all(out_dir.join("pages/shared"))?;
    std::fs::create_dir_all(out_dir.join("pages/po"))?;
    std::fs::create_dir_all(out_dir.join("pages/ba"))?;
    std::fs::create_dir_all(out_dir.join("pages/dev"))?;

    let mut page_count = 0usize;
    let mut all_pages: Vec<PageEntry> = Vec::new();
    let mut nav: BTreeMap<String, Vec<NavEntry>> = BTreeMap::new();
    let llm_enriched = input.llm_summaries.is_some();

    // Shared routes
    let routes_md = pages::shared::render_routes_page(&graph);
    let routes_json = pages::shared::render_routes_json(&graph);
    std::fs::write(out_dir.join("pages/shared/routes.md"), &routes_md)?;
    std::fs::write(
        out_dir.join("pages/shared/routes.json"),
        serde_json::to_string_pretty(&routes_json)?,
    )?;
    all_pages.push(PageEntry {
        slug: "shared/routes".into(),
        role: "shared".into(),
        title: "API Routes".into(),
        kind: "routes".into(),
        path: "pages/shared/routes.md".into(),
        json_path: Some("pages/shared/routes.json".into()),
        community_id: None,
    });
    page_count += 1;

    // PO pages
    let po_index_md = pages::po::render_po_index(&graph, &slug_map, llm_enriched);
    std::fs::write(out_dir.join("pages/po/index.md"), &po_index_md)?;
    nav.entry("po".into()).or_default().push(NavEntry {
        slug: "po/index".into(),
        title: "System Overview".into(),
        kind: "index".into(),
    });
    all_pages.push(PageEntry {
        slug: "po/index".into(),
        role: "po".into(),
        title: "System Overview".into(),
        kind: "index".into(),
        path: "pages/po/index.md".into(),
        json_path: None,
        community_id: None,
    });
    page_count += 1;

    for comm in &graph.community_nodes {
        let comm_id = comm.id.as_str();
        let slug = slug_map.get(comm_id).cloned().unwrap_or_else(|| comm_id.to_string());
        let llm = input.llm_summaries.as_ref().and_then(|m| m.get(comm_id));
        let md = pages::po::render_po_community(&graph, comm, &slug_map, llm);
        std::fs::write(out_dir.join(format!("pages/po/{slug}.md")), &md)?;
        nav.entry("po".into()).or_default().push(NavEntry {
            slug: format!("po/{slug}"),
            title: comm.name.clone(),
            kind: "community".into(),
        });
        all_pages.push(PageEntry {
            slug: format!("po/{slug}"),
            role: "po".into(),
            title: comm.name.clone(),
            kind: "community".into(),
            path: format!("pages/po/{slug}.md"),
            json_path: None,
            community_id: Some(comm_id.to_string()),
        });
        page_count += 1;
    }

    // BA pages
    let ba_index_md = pages::ba::render_ba_index(&graph);
    std::fs::write(out_dir.join("pages/ba/index.md"), &ba_index_md)?;
    nav.entry("ba".into()).or_default().push(NavEntry {
        slug: "ba/index".into(),
        title: "Workflow Overview".into(),
        kind: "index".into(),
    });
    all_pages.push(PageEntry {
        slug: "ba/index".into(),
        role: "ba".into(),
        title: "Workflow Overview".into(),
        kind: "index".into(),
        path: "pages/ba/index.md".into(),
        json_path: None,
        community_id: None,
    });
    page_count += 1;

    for comm in &graph.community_nodes {
        let comm_id = comm.id.as_str();
        let slug = slug_map.get(comm_id).cloned().unwrap_or_else(|| comm_id.to_string());
        let llm = input.llm_summaries.as_ref().and_then(|m| m.get(comm_id));
        let md = pages::ba::render_ba_community(&graph, comm, &slug_map, llm);
        let json_val = pages::ba::render_ba_community_json(&graph, comm);
        std::fs::write(out_dir.join(format!("pages/ba/{slug}.md")), &md)?;
        std::fs::write(
            out_dir.join(format!("pages/ba/{slug}.json")),
            serde_json::to_string_pretty(&json_val)?,
        )?;
        nav.entry("ba".into()).or_default().push(NavEntry {
            slug: format!("ba/{slug}"),
            title: comm.name.clone(),
            kind: "community".into(),
        });
        all_pages.push(PageEntry {
            slug: format!("ba/{slug}"),
            role: "ba".into(),
            title: comm.name.clone(),
            kind: "community".into(),
            path: format!("pages/ba/{slug}.md"),
            json_path: Some(format!("pages/ba/{slug}.json")),
            community_id: Some(comm_id.to_string()),
        });
        page_count += 1;
    }

    // Dev pages
    let dev_index_md = pages::dev::render_dev_index(
        &graph,
        input.repo_map.as_ref(),
        input.unresolved_report.as_deref(),
    );
    std::fs::write(out_dir.join("pages/dev/index.md"), &dev_index_md)?;
    nav.entry("dev".into()).or_default().push(NavEntry {
        slug: "dev/index".into(),
        title: "Technical Overview".into(),
        kind: "index".into(),
    });
    all_pages.push(PageEntry {
        slug: "dev/index".into(),
        role: "dev".into(),
        title: "Technical Overview".into(),
        kind: "index".into(),
        path: "pages/dev/index.md".into(),
        json_path: None,
        community_id: None,
    });
    page_count += 1;

    for comm in &graph.community_nodes {
        let comm_id = comm.id.as_str();
        let slug = slug_map.get(comm_id).cloned().unwrap_or_else(|| comm_id.to_string());
        let llm = input.llm_summaries.as_ref().and_then(|m| m.get(comm_id));
        let md = pages::dev::render_dev_community(&graph, comm, &slug_map, llm);
        let json_val = pages::dev::render_dev_community_json(&graph, comm);
        std::fs::write(out_dir.join(format!("pages/dev/{slug}.md")), &md)?;
        std::fs::write(
            out_dir.join(format!("pages/dev/{slug}.json")),
            serde_json::to_string_pretty(&json_val)?,
        )?;
        nav.entry("dev".into()).or_default().push(NavEntry {
            slug: format!("dev/{slug}"),
            title: comm.name.clone(),
            kind: "community".into(),
        });
        all_pages.push(PageEntry {
            slug: format!("dev/{slug}"),
            role: "dev".into(),
            title: comm.name.clone(),
            kind: "community".into(),
            path: format!("pages/dev/{slug}.md"),
            json_path: Some(format!("pages/dev/{slug}.json")),
            community_id: Some(comm_id.to_string()),
        });
        page_count += 1;
    }

    for role in ["po", "ba", "dev"] {
        nav.entry(role.into()).or_default();
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
        llm_enriched: if llm_enriched { Some(true) } else { None },
        llm_model: input.llm_model,
    };

    let manifest_path = out_dir.join("manifest.json");
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;

    Ok(WikiOutcome {
        out_dir: out_dir.to_path_buf(),
        manifest_path,
        page_count,
        community_count: graph.community_nodes.len(),
        route_count: graph.routes.len(),
        llm_enriched,
    })
}

fn count_unresolved_refs(report: Option<&str>) -> usize {
    report
        .and_then(|r| {
            r.lines()
                .find(|l| l.contains("**Total:**"))
                .and_then(|l| {
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
            matches!(
                n.kind,
                NodeKind::Class | NodeKind::Interface
            ) && node_stereotype(n) == Some("test")
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
            llm_model: None,
        }
    }

    #[test]
    fn generate_wiki_writes_expected_files() {
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
        assert!(out.join("pages/shared/routes.md").exists(), "routes.md");
        assert!(out.join("pages/po/index.md").exists(), "po/index.md");
        assert!(out.join("pages/po/order-service.md").exists(), "po/order-service.md");
        assert!(out.join("pages/ba/index.md").exists(), "ba/index.md");
        assert!(out.join("pages/ba/order-service.md").exists(), "ba/order-service.md");
        assert!(out.join("pages/dev/index.md").exists(), "dev/index.md");
        assert!(out.join("pages/dev/order-service.md").exists(), "dev/order-service.md");
        assert_eq!(outcome.community_count, 1);

        let manifest_json = std::fs::read_to_string(out.join("manifest.json")).unwrap();
        let manifest: WikiManifest = serde_json::from_str(&manifest_json).unwrap();
        assert_eq!(manifest.schema_version, 1);
        assert_eq!(manifest.repo_name, "test-service");
        assert!(manifest.llm_enriched.is_none(), "llm_enriched absent when not enriched");

        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn generate_wiki_records_llm_model_in_manifest_when_enriched() {
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
            llm_model: Some("claude-haiku-4-5-20251001".to_string()),
        };
        let outcome = generate_wiki(input, &out).unwrap();

        let manifest_json = std::fs::read_to_string(out.join("manifest.json")).unwrap();
        let manifest: WikiManifest = serde_json::from_str(&manifest_json).unwrap();
        assert_eq!(manifest.llm_enriched, Some(true));
        assert_eq!(
            manifest.llm_model.as_deref(),
            Some("claude-haiku-4-5-20251001")
        );
        assert!(outcome.llm_enriched);

        let po_page = std::fs::read_to_string(
            out.join("pages/po/payment-service.md")
        ).unwrap();
        assert!(po_page.contains("## Overview"), "po page has overview");
        assert!(po_page.contains("Handles payments"), "po page has llm text");

        let _ = std::fs::remove_dir_all(&out);
    }
}
