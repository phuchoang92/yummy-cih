use super::*;

fn sample_manifest() -> WikiManifest {
    WikiManifest {
        schema_version: 1,
        generated_at: "2026-06-16T10:00:00Z".to_string(),
        repo_name: "test-service".to_string(),
        graph_version: "abc123".to_string(),
        community_version: "def456".to_string(),
        stats: WikiStats {
            community_count: 2,
            route_count: 5,
            process_count: 1,
            class_count: 42,
            test_class_count: 8,
            unresolved_ref_count: 3,
            feature_count: 1,
        },
        roles: vec!["po".into(), "ba".into(), "dev".into()],
        nav: BTreeMap::new(),
        pages: vec![],
        llm: None,
        generation: None,
        module_tree_path: None,
        wiki_meta_path: None,
        warnings: vec![],
    }
}

#[test]
fn manifest_round_trips_json() {
    let m = sample_manifest();
    let json = serde_json::to_string(&m).unwrap();
    let m2: WikiManifest = serde_json::from_str(&json).unwrap();
    assert_eq!(m2.schema_version, 1);
    assert_eq!(m2.repo_name, "test-service");
    assert_eq!(m2.stats.community_count, 2);
    assert_eq!(m2.roles, vec!["po", "ba", "dev"]);
}

#[test]
fn manifest_llm_fields_absent_when_not_enriched() {
    let m = sample_manifest();
    let json = serde_json::to_string(&m).unwrap();
    assert!(!json.contains("\"llm\""), "llm should be absent");
}

#[test]
fn manifest_llm_fields_present_when_enriched() {
    let mut m = sample_manifest();
    m.llm = Some(WikiLlmInfo {
        provider: "anthropic".into(),
        model: "claude-haiku-4-5-20251001".into(),
        language: "en".into(),
        evidence_file_count: 1,
        enriched_community_count: 4,
        failed_community_count: 2,
        failed_community_ids: vec!["Community:1".into(), "Community:2".into()],
    });
    let json = serde_json::to_string(&m).unwrap();
    assert!(json.contains("\"llm\""), "llm should be present");
    assert!(
        json.contains("claude-haiku"),
        "model name should be present"
    );
    assert!(json.contains("failed_community_count"));
}

#[test]
fn manifest_generation_fields_are_optional_and_round_trip() {
    let mut m = sample_manifest();
    m.generation = Some(WikiGenerationInfo {
        mode: "llm-full".into(),
        grouping: "graph".into(),
        review_required: false,
        html_viewer: true,
        incremental: true,
    });
    m.module_tree_path = Some("module_tree.json".into());
    m.wiki_meta_path = Some("wiki_meta.json".into());
    m.warnings.push("fallback used".into());
    let json = serde_json::to_string(&m).unwrap();
    assert!(json.contains("\"generation\""));
    assert!(json.contains("module_tree.json"));
    let decoded: WikiManifest = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.generation.as_ref().unwrap().mode, "llm-full");
    assert_eq!(decoded.warnings, vec!["fallback used"]);
}
