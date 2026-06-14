use super::*;
use crate::analyze::{analyze_emit, analyze_from_scope, extract_jar_api};
use crate::db::LoadOutcome;
use crate::discover::run_discover_core;
use crate::scope::{ScopeFile, ScopeRequest};
use cih_core::JarInfo;
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
        "package com.example;\nclass OwnerController {\n  private OwnerService service;\n  public void handle() { service.findAll(); }\n}\n",
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
    let (nodes, _edges, failed) = extract_jar_api(&[jar.clone()], &fqcns);
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

    let discover = run_discover_core(&root).unwrap();
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
