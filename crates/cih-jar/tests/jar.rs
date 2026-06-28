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
