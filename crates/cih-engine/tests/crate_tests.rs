use cih_core::JarInfo;
use cih_engine::analyze::{analyze_emit, analyze_from_scope, extract_jar_api};
use cih_engine::db::LoadOutcome;
use cih_engine::discover::run_discover_core;
use cih_engine::scan;
use cih_engine::scope::{ScopeFile, ScopeRequest};
use cih_engine::wiki as wiki_cmd;
use cih_wiki::{WikiManifest, WikiMeta};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_ID: AtomicU64 = AtomicU64::new(0);

fn temp_repo() -> PathBuf {
    let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("cih-emit-test-{}-{id}", std::process::id()));
    fs::create_dir_all(root.join("src/main/java/com/example")).unwrap();
    write(
        &root,
        "pom.xml",
        "<project><groupId>com.example</groupId><artifactId>demo</artifactId></project>",
    );
    write(
        &root,
        "src/main/java/com/example/OwnerService.java",
        "package com.example;\n@Service\nclass OwnerService {\n  public void findAll() {}\n}\n",
    );
    write(
        &root,
        "src/main/java/com/example/OwnerController.java",
        "package com.example;\nimport com.example.OwnerService;\nclass OwnerController {\n  private OwnerService service;\n  public void handle() { service.findAll(); }\n}\n",
    );
    root
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn all_scope() -> ScopeRequest {
    ScopeRequest {
        all: true,
        ..ScopeRequest::default()
    }
}

#[test]
fn analyze_emit_writes_artifacts_without_a_database() {
    let root = temp_repo();
    let scan = scan::scan_repo(&root).unwrap();
    let emit = analyze_emit(&scan, all_scope()).unwrap();

    // Structure was emitted and the JSONL artifacts exist on disk.
    assert!(emit.node_count > 0 && emit.edge_count > 0);
    assert_eq!(emit.skipped_count, 0);
    let nodes_jsonl = emit.artifacts_dir.join("nodes.jsonl");
    let edges_jsonl = emit.artifacts_dir.join("edges.jsonl");
    assert!(nodes_jsonl.exists(), "nodes.jsonl should exist");
    assert!(edges_jsonl.exists(), "edges.jsonl should exist");
    assert_eq!(
        fs::read_to_string(&nodes_jsonl).unwrap().lines().count(),
        emit.node_count
    );
    assert!(emit.resolved_edge_count > 0);
    let edges = fs::read_to_string(&edges_jsonl).unwrap();
    assert!(
        edges.contains("\"kind\":\"Calls\"")
            && edges.contains("Method:com.example.OwnerController#handle/0")
            && edges.contains("Method:com.example.OwnerService#findAll/0"),
        "resolved CALLS edge should be written"
    );

    // Unresolved-ref report files exist on disk.
    let unresolved_jsonl = emit.artifacts_dir.join("unresolved-refs.jsonl");
    let unresolved_md = emit.artifacts_dir.join("unresolved-refs.md");
    assert!(
        unresolved_jsonl.exists(),
        "unresolved-refs.jsonl should exist"
    );
    assert!(unresolved_md.exists(), "unresolved-refs.md should exist");
    assert!(
        fs::read_to_string(&unresolved_md)
            .unwrap()
            .contains("# Unresolved References"),
        "unresolved-refs.md should have expected header"
    );

    // Skipped (no DB) maps to the right summary status, no exit.
    let summary = emit.summary(&LoadOutcome::Skipped);
    assert_eq!(summary.falkor_status, "skipped");
    assert!(summary.falkor_error.is_none());

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn content_version_is_stable_for_identical_content() {
    let root = temp_repo();
    let first = {
        let scan = scan::scan_repo(&root).unwrap();
        analyze_emit(&scan, all_scope()).unwrap().version
    };
    let second = {
        let scan = scan::scan_repo(&root).unwrap();
        analyze_emit(&scan, all_scope()).unwrap().version
    };
    assert_eq!(first, second, "same content must yield the same version");

    // Changing a file body changes the version + relocates the artifacts dir.
    write(
        &root,
        "src/main/java/com/example/OwnerService.java",
        "package com.example;\n@Service\nclass OwnerService {\n  public void findAll() {}\n  public void findOne() {}\n}\n",
    );
    let scan = scan::scan_repo(&root).unwrap();
    let changed = analyze_emit(&scan, all_scope()).unwrap();
    assert_ne!(
        changed.version, first,
        "changed content must yield a new version"
    );

    // Prune keeps only the current version dir.
    let artifacts_parent = root.join(".cih").join("artifacts");
    let dirs: Vec<String> = fs::read_dir(&artifacts_parent)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(dirs, vec![changed.version.clone()]);

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn incremental_noop_when_files_unchanged() {
    let root = temp_repo();
    let scan = scan::scan_repo(&root).unwrap();
    let first = analyze_emit(&scan, all_scope()).unwrap();
    assert!(!first.reused_artifacts);
    assert!(first.cache_stats.enabled);

    let scan = scan::scan_repo(&root).unwrap();
    let second = analyze_emit(&scan, all_scope()).unwrap();

    assert!(second.reused_artifacts);
    assert!(second.cache_stats.noop);
    assert_eq!(second.version, first.version);
    assert_eq!(second.cache_stats.reparsed_files, 0);

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn incremental_bumps_version_on_single_file_change() {
    let root = temp_repo();
    let scan = scan::scan_repo(&root).unwrap();
    let first = analyze_emit(&scan, all_scope()).unwrap();

    write(
        &root,
        "src/main/java/com/example/OwnerService.java",
        "package com.example;\n@Service\nclass OwnerService {\n  public void findAll() {}\n  public void findOne() {}\n}\n",
    );
    let scan = scan::scan_repo(&root).unwrap();
    let changed = analyze_emit(&scan, all_scope()).unwrap();

    assert!(!changed.reused_artifacts);
    assert_ne!(changed.version, first.version);
    assert_eq!(changed.cache_stats.changed_files, 1);
    assert_eq!(changed.cache_stats.reparsed_files, 2);
    assert_eq!(changed.cache_stats.expanded_files, 2);

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn ir_only_body_change_bumps_version() {
    // Verifies that a method-body edit (new call site, no new declarations) changes
    // the content_version, proving post-resolve versioning covers the IR.
    let root = temp_repo();
    let scan = scan::scan_repo(&root).unwrap();
    let v1 = analyze_emit(&scan, all_scope()).unwrap().version;

    // Replace handle() body with a different call — same method signature, new reference.
    write(
        &root,
        "src/main/java/com/example/OwnerController.java",
        "package com.example;\nclass OwnerController {\n  private OwnerService service;\n  public void handle() { service.findAll(); service.findAll(); }\n}\n",
    );
    let scan2 = scan::scan_repo(&root).unwrap();
    let v2 = analyze_emit(&scan2, all_scope()).unwrap().version;
    assert_ne!(
        v1, v2,
        "adding a call in a method body must bump the version"
    );

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn resolve_subcommand_reads_saved_scope() {
    let root = temp_repo();
    // First run analyze to produce .cih/scope.json.
    let scan = scan::scan_repo(&root).unwrap();
    let v1 = analyze_emit(&scan, all_scope()).unwrap().version;

    // resolve subcommand reads scope.json and re-runs — same content → same version.
    let scope_path = root.join(".cih").join("scope.json");
    let raw = fs::read_to_string(&scope_path).unwrap();
    let scope_file: ScopeFile = serde_json::from_str(&raw).unwrap();
    let v2 = analyze_from_scope(scope_file, scope_path, &[])
        .unwrap()
        .version;
    assert_eq!(
        v1, v2,
        "resolve with same scope must produce the same version"
    );

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn extract_jar_api_returns_empty_for_empty_inputs() {
    let (nodes, edges, failed) = extract_jar_api(&[], &["com.example.Lib".to_string()]);
    assert!(nodes.is_empty());
    assert!(edges.is_empty());
    assert_eq!(failed, 0);

    let (nodes, edges, failed) = extract_jar_api(&[], &[]);
    assert!(nodes.is_empty());
    assert!(edges.is_empty());
    assert_eq!(failed, 0);
}

#[test]
fn extract_jar_api_demand_driven_with_sample_jar() {
    // Uses the cih-jar sample fixture. Skip gracefully if it's absent (rare).
    let sample_jar = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("cih-jar")
        .join("tests")
        .join("fixtures")
        .join("sample.jar");
    if !sample_jar.exists() {
        return;
    }

    let jar = JarInfo {
        path: sample_jar.to_string_lossy().into_owned(),
        group_id: Some("com.acme".into()),
        artifact: Some("sample".into()),
        is_own: false,
        classes: 1,
    };

    // Request only com.acme.Sample — demand-driven.
    let fqcns = vec!["com.acme.Sample".to_string()];
    let (nodes, _edges, failed) = extract_jar_api(std::slice::from_ref(&jar), &fqcns);
    assert_eq!(failed, 0);
    assert!(
        !nodes.is_empty(),
        "should have extracted the Sample class node"
    );
    assert!(
        nodes
            .iter()
            .any(|n| n.id.as_str() == "Class:com.acme.Sample"),
        "expected Class:com.acme.Sample node"
    );

    // Requesting a different FQCN that isn't in the JAR → no nodes, no failure.
    let (nodes2, _, failed2) = extract_jar_api(&[jar], &["com.other.Missing".to_string()]);
    assert_eq!(failed2, 0);
    assert!(nodes2.is_empty(), "unknown class should produce no nodes");

    // Unresolvable path → failed counter increments.
    let bad_jar = JarInfo {
        path: "/nonexistent/path/foo.jar".into(),
        group_id: None,
        artifact: None,
        is_own: false,
        classes: 0,
    };
    let (_, _, failed3) = extract_jar_api(&[bad_jar], &fqcns);
    assert_eq!(failed3, 1);
}

#[test]
fn jar_nodes_appear_in_graph_artifacts() {
    let sample_jar = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("cih-jar")
        .join("tests")
        .join("fixtures")
        .join("sample.jar");
    if !sample_jar.exists() {
        return;
    }

    let root = temp_repo();
    // Inject a source file that calls com.acme.Sample (→ unresolved external FQCN).
    write(
        &root,
        "src/main/java/com/example/UsesExternal.java",
        "package com.example;\nimport com.acme.Sample;\nclass UsesExternal {\n  void run() { new Sample(42); }\n}\n",
    );

    let mut scan = scan::scan_repo(&root).unwrap();
    // Inject the sample JAR into the repo-map so analyze_from_scope can see it.
    scan.repo_map.jars.push(JarInfo {
        path: sample_jar.to_string_lossy().into_owned(),
        group_id: Some("com.acme".into()),
        artifact: Some("sample".into()),
        is_own: false,
        classes: 1,
    });

    let emit = analyze_emit(&scan, all_scope()).unwrap();
    assert!(
        emit.jar_node_count > 0,
        "expected JAR nodes in the output; got 0"
    );

    // The Class:com.acme.Sample node should appear in nodes.jsonl.
    let nodes_jsonl = fs::read_to_string(emit.artifacts_dir.join("nodes.jsonl")).unwrap();
    assert!(
        nodes_jsonl.contains("Class:com.acme.Sample"),
        "Class:com.acme.Sample should be in nodes.jsonl"
    );

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn test_class_emits_tests_edges_in_artifacts() {
    let root = temp_repo();
    // Add a test class with @SpringBootTest, @MockBean, and @Test method.
    write(
        &root,
        "src/test/java/com/example/OwnerServiceTest.java",
        r#"package com.example;
import org.springframework.boot.test.context.SpringBootTest;
import org.springframework.boot.test.mock.mockito.MockBean;
@SpringBootTest
public class OwnerServiceTest {
    @MockBean
    private OwnerService ownerService;
    @Test
    public void testFindAll() {}
}
"#,
    );
    let scan = scan::scan_repo(&root).unwrap();
    let emit = analyze_emit(&scan, all_scope()).unwrap();

    let nodes_jsonl = fs::read_to_string(emit.artifacts_dir.join("nodes.jsonl")).unwrap();
    let edges_jsonl = fs::read_to_string(emit.artifacts_dir.join("edges.jsonl")).unwrap();

    // Test class node must exist with stereotype=test.
    assert!(
        nodes_jsonl.contains("OwnerServiceTest"),
        "test class node should appear in nodes.jsonl"
    );
    // TESTS edges must be emitted.
    assert!(
        edges_jsonl.contains("\"kind\":\"Tests\""),
        "TESTS edges should appear in edges.jsonl"
    );

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn discover_emits_community_and_process_artifacts() {
    let root = temp_repo();
    write(
        &root,
        "src/main/java/com/example/OwnerService.java",
        "package com.example;\n@Service\nclass OwnerService {\n  public void findAll() { helper(); }\n  private void helper() {}\n}\n",
    );
    let scan = scan::scan_repo(&root).unwrap();
    let analyze = analyze_emit(&scan, all_scope()).unwrap();
    assert!(analyze.resolved_edge_count >= 2);

    let discover = run_discover_core(
        &root,
        &cih_engine::discover::DiscoverOverrides {
            min_community_size: Some(1),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(discover.artifacts_dir.join("nodes.jsonl").exists());
    assert!(discover.artifacts_dir.join("edges.jsonl").exists());
    assert!(
        discover.community_count >= 1,
        "expected at least one detected community"
    );
    assert!(
        discover.process_count >= 1,
        "expected at least one 3-step process trace"
    );

    let nodes_jsonl = fs::read_to_string(discover.artifacts_dir.join("nodes.jsonl")).unwrap();
    assert!(nodes_jsonl.contains("\"kind\":\"Community\""));
    assert!(nodes_jsonl.contains("\"kind\":\"Process\""));

    fs::remove_dir_all(&root).unwrap();
}

fn repo_with_wiki_artifacts() -> PathBuf {
    let root = temp_repo();
    let scan = scan::scan_repo(&root).unwrap();
    analyze_emit(&scan, all_scope()).unwrap();
    run_discover_core(
        &root,
        &cih_engine::discover::DiscoverOverrides {
            min_community_size: Some(1),
            ..Default::default()
        },
    )
    .unwrap();
    root
}

#[test]
fn wiki_command_graph_only_writes_manifest_without_llm_metadata() {
    let root = repo_with_wiki_artifacts();
    wiki_cmd::run_wiki(wiki_cmd::WikiConfig {
        repo: root.clone(),
        wiki_mode: wiki_cmd::WikiMode::Graph,
        grouping: wiki_cmd::WikiGrouping::Graph,
        ..wiki_cmd::WikiConfig::default()
    })
    .unwrap();

    let manifest_json = fs::read_to_string(root.join(".cih/wiki/manifest.json")).unwrap();
    let manifest: WikiManifest = serde_json::from_str(&manifest_json).unwrap();
    assert!(
        manifest.llm.is_none(),
        "graph-only wiki must omit llm metadata"
    );

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn wiki_command_dry_run_llm_writes_metadata_without_api_key() {
    let root = repo_with_wiki_artifacts();
    wiki_cmd::run_wiki(wiki_cmd::WikiConfig {
        repo: root.clone(),
        run_llm: true,
        llm: wiki_cmd::LlmCallConfig {
            model: "dry-model".into(),
            api_key_env: Some(format!(
                "CIH_TEST_MISSING_KEY_{}",
                TEST_ID.load(Ordering::Relaxed)
            )),
            ..Default::default()
        },
        llm_dry_run: true,
        wiki_language: "vi".into(),
        wiki_mode: wiki_cmd::WikiMode::LlmSummary,
        ..wiki_cmd::WikiConfig::default()
    })
    .unwrap();

    let manifest_json = fs::read_to_string(root.join(".cih/wiki/manifest.json")).unwrap();
    let manifest: WikiManifest = serde_json::from_str(&manifest_json).unwrap();
    let llm = manifest.llm.expect("dry-run llm metadata");
    assert_eq!(llm.provider, "openai-compatible");
    assert_eq!(llm.model, "dry-model");
    assert_eq!(llm.language, "vi");
    assert_eq!(llm.failed_community_count, 0);

    let meta_json = fs::read_to_string(root.join(".cih/wiki/wiki_meta.json")).unwrap();
    let meta: WikiMeta = serde_json::from_str(&meta_json).unwrap();
    assert!(
        !meta.feature_cache.is_empty(),
        "dry-run feature summaries should be cached after generate_wiki rewrites wiki_meta.json"
    );

    wiki_cmd::run_wiki(wiki_cmd::WikiConfig {
        repo: root.clone(),
        run_llm: true,
        llm: wiki_cmd::LlmCallConfig {
            model: "dry-model".into(),
            api_key_env: Some(format!(
                "CIH_TEST_MISSING_KEY_{}",
                TEST_ID.load(Ordering::Relaxed)
            )),
            ..Default::default()
        },
        llm_dry_run: true,
        wiki_language: "vi".into(),
        wiki_mode: wiki_cmd::WikiMode::LlmSummary,
        incremental: true,
        ..wiki_cmd::WikiConfig::default()
    })
    .unwrap();

    let meta_json = fs::read_to_string(root.join(".cih/wiki/wiki_meta.json")).unwrap();
    let meta_after_incremental: WikiMeta = serde_json::from_str(&meta_json).unwrap();
    assert_eq!(
        meta.feature_cache, meta_after_incremental.feature_cache,
        "incremental dry-run should preserve cached feature summaries after metadata rewrite"
    );

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn wiki_command_http_json_requires_provider_config() {
    let root = repo_with_wiki_artifacts();
    let err = wiki_cmd::run_wiki(wiki_cmd::WikiConfig {
        repo: root.clone(),
        run_llm: true,
        llm: wiki_cmd::LlmCallConfig {
            provider: wiki_cmd::LlmProvider::HttpJson,
            base_url: "http://localhost".into(),
            model: "local".into(),
            ..Default::default()
        },
        llm_dry_run: true,
        wiki_mode: wiki_cmd::WikiMode::LlmSummary,
        ..wiki_cmd::WikiConfig::default()
    })
    .unwrap_err()
    .to_string();
    assert!(err.contains("--llm-provider-config"));

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn analyze_emit_writes_db_query_and_table_artifacts() {
    let root = temp_repo();
    write(
        &root,
        "src/main/java/com/bank/OverdraftAdapterImpl.java",
        r#"package com.bank;
import java.sql.Connection;
public class OverdraftAdapterImpl {
    private static final String QUERY_GET_TYPE =
        "SELECT code, desc FROM CUSTOM_OVERDRAFT_TYPE WHERE code = ?";
    private static final String QUERY_INSERT =
        "INSERT INTO CUSTOM_OVERDRAFT (id, amount) VALUES (?, ?)";

    public Object getType(Connection conn, String code) {
        return DBUtil.executeQuery(conn, QUERY_GET_TYPE, code);
    }

    public void insert(Connection conn, long id, long amount) {
        DBUtil.executeUpdate(conn, QUERY_INSERT, id, amount);
    }
}
"#,
    );

    let scan = scan::scan_repo(&root).unwrap();
    let emit = analyze_emit(&scan, all_scope()).unwrap();

    let nodes_content = fs::read_to_string(emit.artifacts_dir.join("nodes.jsonl")).unwrap();
    let edges_content = fs::read_to_string(emit.artifacts_dir.join("edges.jsonl")).unwrap();

    assert!(
        nodes_content.contains("\"kind\":\"DbQuery\""),
        "nodes.jsonl should contain DbQuery nodes"
    );
    assert!(
        nodes_content.contains("\"kind\":\"DbTable\""),
        "nodes.jsonl should contain DbTable nodes"
    );
    assert!(
        nodes_content.contains("CUSTOM_OVERDRAFT_TYPE"),
        "DbTable for CUSTOM_OVERDRAFT_TYPE should be present"
    );
    assert!(
        nodes_content.contains("CUSTOM_OVERDRAFT"),
        "DbTable for CUSTOM_OVERDRAFT should be present"
    );
    assert!(
        edges_content.contains("\"kind\":\"ExecutesQuery\""),
        "EXECUTES_QUERY edges should be present"
    );
    assert!(
        edges_content.contains("\"kind\":\"ReadsTable\""),
        "READS_TABLE edge for SELECT should be present"
    );
    assert!(
        edges_content.contains("\"kind\":\"WritesTable\""),
        "WRITES_TABLE edge for INSERT should be present"
    );

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn discover_preserves_analyze_artifacts_on_disk() {
    let root = temp_repo();
    write(
        &root,
        "src/main/java/com/example/OwnerService.java",
        "package com.example;\n@Service\nclass OwnerService {\n  public void findAll() { helper(); }\n  private void helper() {}\n}\n",
    );
    let scan = scan::scan_repo(&root).unwrap();
    let analyze = analyze_emit(&scan, all_scope()).unwrap();

    let analyze_nodes = analyze.artifacts.nodes_path.clone();
    let analyze_edges = analyze.artifacts.edges_path.clone();
    let analyze_version = analyze.artifacts.version.to_string();

    run_discover_core(&root, &cih_engine::discover::DiscoverOverrides::default()).unwrap();

    assert!(
        analyze_nodes.exists(),
        "analyze nodes.jsonl must survive discover"
    );
    assert!(
        analyze_edges.exists(),
        "analyze edges.jsonl must survive discover"
    );

    let latest = cih_engine::versioning::latest_graph_artifacts(&root).unwrap();
    assert_eq!(
        latest.version.as_str(),
        analyze_version,
        "latest_graph_artifacts must still return the analyze version after discover"
    );
    assert!(
        latest.nodes_path.to_string_lossy().contains("artifacts/"),
        "latest_graph_artifacts path must be under .cih/artifacts/, not artifacts-community/"
    );

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn discover_outcome_source_artifacts_point_to_analyze_dir() {
    let root = temp_repo();
    write(
        &root,
        "src/main/java/com/example/OwnerService.java",
        "package com.example;\n@Service\nclass OwnerService {\n  public void findAll() { helper(); }\n  private void helper() {}\n}\n",
    );
    let scan = scan::scan_repo(&root).unwrap();
    analyze_emit(&scan, all_scope()).unwrap();

    let discover =
        run_discover_core(&root, &cih_engine::discover::DiscoverOverrides::default()).unwrap();

    assert!(
        discover
            .source_artifacts
            .nodes_path
            .to_string_lossy()
            .contains("artifacts/"),
        "source_artifacts must be under .cih/artifacts/"
    );
    assert!(
        !discover
            .source_artifacts
            .nodes_path
            .to_string_lossy()
            .contains("artifacts-community"),
        "source_artifacts must NOT be under .cih/artifacts-community/"
    );
    assert!(
        discover
            .artifacts
            .nodes_path
            .to_string_lossy()
            .contains("artifacts-community"),
        "discover.artifacts must be under .cih/artifacts-community/"
    );
    assert_ne!(
        discover.source_artifacts.version, discover.artifacts.version,
        "source and community versions must differ"
    );

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn discover_load_artifacts_are_analyze_then_community() {
    let root = temp_repo();
    write(
        &root,
        "src/main/java/com/example/OwnerService.java",
        "package com.example;\n@Service\nclass OwnerService {\n  public void findAll() { helper(); }\n  private void helper() {}\n}\n",
    );
    let scan = scan::scan_repo(&root).unwrap();
    let analyze = analyze_emit(&scan, all_scope()).unwrap();

    let discover =
        run_discover_core(&root, &cih_engine::discover::DiscoverOverrides::default()).unwrap();
    let artifact_sets = discover.artifact_sets_for_load();

    // Canonicalize both sides: macOS temp_dir() symlinks may differ from canonicalized paths.
    assert_eq!(
        artifact_sets[0].nodes_path.canonicalize().unwrap(),
        analyze.artifacts.nodes_path.canonicalize().unwrap()
    );
    assert_eq!(
        artifact_sets[0].edges_path.canonicalize().unwrap(),
        analyze.artifacts.edges_path.canonicalize().unwrap()
    );
    assert_eq!(artifact_sets[0].version, analyze.artifacts.version);

    assert_eq!(artifact_sets[1].nodes_path, discover.artifacts.nodes_path);
    assert_eq!(artifact_sets[1].edges_path, discover.artifacts.edges_path);
    assert_eq!(artifact_sets[1].version, discover.artifacts.version);

    fs::remove_dir_all(&root).unwrap();
}
