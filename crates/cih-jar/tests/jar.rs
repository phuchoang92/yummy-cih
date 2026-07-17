use std::collections::HashSet;
use std::path::PathBuf;

use cih_core::{constructor_id, field_id, method_id, type_id, EdgeKind, NodeId, NodeKind};
use cih_jar::{JarApiExtractor, JarApiOutput};

fn sample_jar() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sample.jar"
    ))
}

fn has_node(out: &JarApiOutput, id: &NodeId) -> bool {
    out.nodes.iter().any(|n| &n.id == id)
}

fn has_edge(out: &JarApiOutput, kind: EdgeKind, src: &NodeId, dst: &NodeId) -> bool {
    out.edges
        .iter()
        .any(|e| e.kind == kind && &e.src == src && &e.dst == dst)
}

#[test]
fn extracts_api_with_ids_matching_the_locked_scheme() {
    let out = JarApiExtractor::all().extract(&sample_jar()).unwrap();
    assert!(out.skipped.is_empty(), "skipped: {:?}", out.skipped);

    let sample = type_id(NodeKind::Class, "com.acme.Sample");
    let inner = type_id(NodeKind::Class, "com.acme.Sample.Inner");

    assert!(has_node(&out, &sample));
    assert!(has_node(&out, &field_id("com.acme.Sample", "count")));
    assert!(has_node(&out, &constructor_id("com.acme.Sample", 1)));
    assert!(has_node(&out, &method_id("com.acme.Sample", "greet", 1)));
    assert!(has_node(&out, &method_id("com.acme.Sample", "make", 0)));
    assert!(has_node(&out, &inner));
    assert!(has_node(
        &out,
        &method_id("com.acme.Sample.Inner", "ping", 0)
    ));

    assert!(has_edge(
        &out,
        EdgeKind::HasMethod,
        &sample,
        &method_id("com.acme.Sample", "greet", 1)
    ));
    assert!(has_edge(
        &out,
        EdgeKind::HasField,
        &sample,
        &field_id("com.acme.Sample", "count")
    ));

    assert!(!has_node(
        &out,
        &type_id(NodeKind::Class, "com.acme.Sample.1")
    ));

    let greet = out
        .nodes
        .iter()
        .find(|n| n.id == method_id("com.acme.Sample", "greet", 1))
        .unwrap();
    let props = greet.props.as_ref().unwrap();
    assert_eq!(props.get("fromJar").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(props.get("external").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        props.get("returns").and_then(|v| v.as_str()),
        Some("java.lang.String")
    );
    assert_eq!(
        props
            .get("params")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>()),
        Some(vec!["int"])
    );
}

#[test]
fn demand_driven_include_emits_only_requested_classes() {
    let include = HashSet::from(["com.acme.Sample.Inner".to_string()]);
    let out = JarApiExtractor::with_include(include)
        .extract(&sample_jar())
        .unwrap();

    assert!(has_node(
        &out,
        &type_id(NodeKind::Class, "com.acme.Sample.Inner")
    ));
    assert!(has_node(
        &out,
        &method_id("com.acme.Sample.Inner", "ping", 0)
    ));
    assert!(!has_node(
        &out,
        &type_id(NodeKind::Class, "com.acme.Sample")
    ));
    assert_eq!(out.classes, 1);
}

// ── Robustness: bad input must skip, never panic or abort the whole JAR ──────

use std::io::Write;

fn unique_dir() -> PathBuf {
    // pid keeps this unique across processes; the counter keeps it unique within
    // one test binary. A wall-clock timestamp is not collision-proof — parallel
    // tests can read the same tick and share a dir, then race on teardown.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "cih-jar-robust-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a jar (zip) with the given entries to a temp path and return it.
fn write_jar(entries: &[(&str, &[u8])]) -> (PathBuf, PathBuf) {
    let dir = unique_dir();
    let path = dir.join("test.jar");
    let file = std::fs::File::create(&path).unwrap();
    let mut zw = zip::ZipWriter::new(file);
    for (name, bytes) in entries {
        zw.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zw.write_all(bytes).unwrap();
    }
    zw.finish().unwrap();
    (path, dir)
}

#[test]
fn nonexistent_jar_path_errors() {
    let err = JarApiExtractor::all()
        .extract(&PathBuf::from("/no/such/file.jar"))
        .unwrap_err();
    assert!(err.to_string().contains("failed to open jar"), "{err}");
}

#[test]
fn non_zip_file_errors() {
    let dir = unique_dir();
    let path = dir.join("garbage.jar");
    std::fs::write(&path, b"this is not a zip archive").unwrap();
    let err = JarApiExtractor::all().extract(&path).unwrap_err();
    assert!(err.to_string().contains("failed to read jar"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn malformed_class_is_skipped_not_fatal() {
    // A .class entry that is not a valid class file must be recorded as skipped,
    // and extraction must still succeed (the documented "never fatal" contract).
    let (path, dir) = write_jar(&[("com/acme/Bad.class", b"\xca\xfe\xba\xbe not really")]);
    let out = JarApiExtractor::all().extract(&path).unwrap();
    assert_eq!(out.nodes.len(), 0, "no nodes from a bad class");
    assert_eq!(out.classes, 0);
    assert_eq!(out.skipped.len(), 1, "the bad class is recorded as skipped");
    assert_eq!(out.skipped[0].entry, "com/acme/Bad.class");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn non_class_entries_are_ignored() {
    // Non-`.class` entries are silently ignored — not emitted, not skipped.
    let (path, dir) = write_jar(&[
        ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0"),
        ("com/acme/readme.txt", b"hello"),
    ]);
    let out = JarApiExtractor::all().extract(&path).unwrap();
    assert!(out.nodes.is_empty());
    assert!(out.skipped.is_empty(), "non-class entries are not skips");
    std::fs::remove_dir_all(&dir).ok();
}
