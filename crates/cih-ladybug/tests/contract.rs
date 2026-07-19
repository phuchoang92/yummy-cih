//! The backend-neutral `GraphStore` contract suite against LadybugDB —
//! **hermetic**: embedded DB over a tempdir, no external service, runs in the
//! default `cargo test --workspace`. The first backend whose contract run
//! needs no docker.

use std::sync::Arc;

use cih_graph_store::GraphStore;
use cih_ladybug::LadybugStore;

#[tokio::test(flavor = "multi_thread")]
async fn ladybug_passes_the_graph_store_contract() {
    let root = std::env::temp_dir().join(format!(
        "cih-ladybug-contract-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    std::fs::create_dir_all(&root).expect("create contract root");
    let root_str = root.to_string_lossy().into_owned();

    cih_graph_store::contract::run_contract_suite(move |graph_key: &str| {
        let store: Arc<dyn GraphStore> = Arc::new(LadybugStore::connect(&root_str, graph_key)?);
        Ok(store)
    })
    .await
    .expect("contract suite infrastructure");

    let _ = std::fs::remove_dir_all(&root);
}

/// Ladybug-specific: a FAILED load must leave `CURRENT` — and this store's
/// own reads — on the previous good version (the flip happens only after a
/// successful load + checkpoint).
#[tokio::test(flavor = "multi_thread")]
async fn failed_load_keeps_previous_version_live() {
    use cih_core::{GraphArtifacts, Node, NodeId, NodeKind, Range, VersionId};

    let root = std::env::temp_dir().join(format!("cih-ladybug-fail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create root");
    let store = LadybugStore::connect(&root.to_string_lossy(), "k").expect("connect");

    let good = Node {
        id: NodeId::new("Method:com.acme.A#a/0"),
        kind: NodeKind::Method,
        name: "a".into(),
        qualified_name: None,
        file: "A.java".into(),
        range: Range::default(),
        props: None,
    };
    let dir = root.join("artifacts");
    let artifacts =
        GraphArtifacts::write(&dir, VersionId::new("v"), std::slice::from_ref(&good), &[])
            .expect("write");
    store.bulk_load(&artifacts).await.expect("good load");
    let current = std::fs::read_to_string(root.join("k/CURRENT")).expect("CURRENT exists");
    assert_eq!(current.trim(), "v1");

    // A load whose artifacts are unreadable must fail without moving CURRENT.
    let broken = GraphArtifacts {
        nodes_path: root.join("nope/nodes.jsonl"),
        edges_path: root.join("nope/edges.jsonl"),
        version: VersionId::new("broken"),
    };
    store
        .bulk_load(&broken)
        .await
        .expect_err("broken artifacts must fail");
    let current = std::fs::read_to_string(root.join("k/CURRENT")).expect("CURRENT survives");
    assert_eq!(current.trim(), "v1", "CURRENT still on the good version");
    let n = store
        .get_node(&good.id)
        .await
        .expect("read after failed load")
        .expect("previous version still serves reads");
    assert_eq!(n.name, "a");

    let _ = std::fs::remove_dir_all(&root);
}
