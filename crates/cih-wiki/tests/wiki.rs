use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};
use cih_wiki::{
    generate_wiki, CommunityLlmSummary, WikiGenerationInfo, WikiInput, WikiLlmInfo, WikiManifest,
};

static TEST_ID: AtomicU64 = AtomicU64::new(0);

fn tmp_dir(label: &str) -> std::path::PathBuf {
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
        feature_llm_summaries: None,
        flow_llm_summaries: None,
        grouping: "graph".to_string(),
        filter_feature: vec![],
        bodies: HashMap::new(),
        feature_of: Box::new(|_, _| "shared".to_string()),
        entrypoints: vec![],
        repo_commit: None,
        flags_hash: None,
        changed_files: None,
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
        props: None,
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
    assert!(
        out.join("pages/shared/index.md").exists(),
        "shared/index.md"
    );
    assert!(out.join("pages/shared/po.md").exists(), "shared/po.md");
    assert!(out.join("pages/shared/ba.md").exists(), "shared/ba.md");
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
    let sym = make_node("Method:com.example.Bar#run/0", NodeKind::Method, "run");
    let comm = make_node("Community:1", NodeKind::Community, "payment-service");
    let comm_edges = [Edge {
        src: sym.id.clone(),
        dst: NodeId::new("Community:1".to_string()),
        kind: EdgeKind::MemberOf,
        confidence: 1.0,
        reason: String::new(),
        props: None,
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
        feature_llm_summaries: None,
        flow_llm_summaries: None,
        grouping: "graph".to_string(),
        filter_feature: vec![],
        bodies: HashMap::new(),
        feature_of: Box::new(|_, _| "shared".to_string()),
        entrypoints: vec![],
        repo_commit: None,
        flags_hash: None,
        changed_files: None,
    };
    let outcome = generate_wiki(input, &out).unwrap();

    let manifest_json = std::fs::read_to_string(out.join("manifest.json")).unwrap();
    let manifest: WikiManifest = serde_json::from_str(&manifest_json).unwrap();
    let llm = manifest.llm.as_ref().expect("llm metadata");
    assert_eq!(Some(llm.model.as_str()), Some("claude-haiku-4-5-20251001"));
    assert_eq!(llm.provider, "anthropic");
    assert!(outcome.llm_enriched);

    let po_page = std::fs::read_to_string(out.join("pages/shared/po.md")).unwrap();
    assert!(po_page.contains("## Overview"), "po page has overview");
    assert!(po_page.contains("Handles payments"), "po page has llm text");

    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn generate_wiki_second_run_writes_zero_pages() {
    let sym = make_node("Method:com.example.Foo#bar/0", NodeKind::Method, "bar");
    let comm = make_node("Community:0", NodeKind::Community, "order-service");
    let comm_edges = [Edge {
        src: sym.id.clone(),
        dst: NodeId::new("Community:0".to_string()),
        kind: EdgeKind::MemberOf,
        confidence: 1.0,
        reason: String::new(),
        props: None,
    }];
    let nodes = [sym];
    let comm_nodes = [comm];

    let out = tmp_dir("determinism");
    // First run — everything is new.
    let first = generate_wiki(minimal_input(&nodes, &[], &comm_nodes, &comm_edges), &out).unwrap();
    assert!(first.pages_written > 0, "first run must write pages");

    // Second run — content is identical; sink should write nothing.
    let second = generate_wiki(minimal_input(&nodes, &[], &comm_nodes, &comm_edges), &out).unwrap();
    assert_eq!(
        second.pages_written, 0,
        "second run should write 0 pages (determinism)"
    );
    assert!(
        second.pages_unchanged > 0,
        "second run should have unchanged pages"
    );

    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn generate_wiki_since_skips_unchanged_features() {
    // Two features: "alpha" and "beta" (two separate communities, each with one method).
    let m_alpha = Node {
        id: NodeId::new("Method:com.alpha.Foo#run/0".to_string()),
        kind: NodeKind::Method,
        name: "run".to_string(),
        qualified_name: None,
        file: "modules/alpha/Foo.java".to_string(),
        range: Range::default(),
        props: None,
    };
    let m_beta = Node {
        id: NodeId::new("Method:com.beta.Bar#go/0".to_string()),
        kind: NodeKind::Method,
        name: "go".to_string(),
        qualified_name: None,
        file: "modules/beta/Bar.java".to_string(),
        range: Range::default(),
        props: None,
    };
    let c_alpha = make_node("Community:alpha", NodeKind::Community, "alpha");
    let c_beta = make_node("Community:beta", NodeKind::Community, "beta");
    let comm_edges = [
        Edge {
            src: m_alpha.id.clone(),
            dst: c_alpha.id.clone(),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        },
        Edge {
            src: m_beta.id.clone(),
            dst: c_beta.id.clone(),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        },
    ];
    let nodes = [m_alpha, m_beta];
    let comm_nodes = [c_alpha, c_beta];

    let out = tmp_dir("since");
    // Full first run.
    generate_wiki(minimal_input(&nodes, &[], &comm_nodes, &comm_edges), &out).unwrap();

    // Partial second run: only "src/alpha/Foo.java" changed.
    let mut changed = std::collections::HashSet::new();
    changed.insert("modules/alpha/Foo.java".to_string());
    let mut input = minimal_input(&nodes, &[], &comm_nodes, &comm_edges);
    input.changed_files = Some(changed);
    let outcome = generate_wiki(input, &out).unwrap();

    // Manifest must contain pages for both features (merged).
    let manifest_json = std::fs::read_to_string(out.join("manifest.json")).unwrap();
    let manifest: WikiManifest = serde_json::from_str(&manifest_json).unwrap();
    let feature_roles: std::collections::HashSet<&str> = manifest
        .pages
        .iter()
        .filter(|p| !matches!(p.role.as_str(), "system" | "shared" | "communities"))
        .map(|p| p.role.as_str())
        .collect();
    assert!(feature_roles.contains("alpha"), "alpha pages in manifest");
    assert!(
        feature_roles.contains("beta"),
        "beta pages in manifest after merge"
    );
    assert!(outcome.page_count >= 2, "at least one page per feature");

    let _ = std::fs::remove_dir_all(&out);
}
