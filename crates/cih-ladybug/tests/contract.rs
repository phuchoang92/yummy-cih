//! The backend-neutral `GraphStore` contract suite against LadybugDB —
//! **hermetic**: embedded DB over a tempdir, no external service, runs in the
//! default `cargo test --workspace`. The first backend whose contract run
//! needs no docker.

use std::sync::Arc;

use cih_core::{
    Edge, EdgeKind, GraphArtifacts, Node, NodeId, NodeKind, Range, RepositoryId, VersionId,
};
use cih_graph_store::publication::{
    ArtifactVersion, CurrentPublication, GraphContentVersion, GraphPublicationEpoch,
    GraphPublicationStore, ManifestDigest, PublicationCasResult, PublisherFencingToken,
    ValidationDigest,
};
use cih_graph_store::{Direction, GraphStore, TransitionQuery};
use cih_ladybug::{LadybugPublicationStore, LadybugStore};

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

#[tokio::test(flavor = "multi_thread")]
async fn ladybug_passes_the_publication_store_contract() {
    let root = tempfile::tempdir().expect("publication root");
    let store =
        Arc::new(LadybugPublicationStore::connect(root.path()).expect("connect publication store"));
    cih_graph_store::publication::contract::run_publication_contract_suite(store)
        .await
        .expect("publication contract suite");
}

fn digest(byte: char) -> String {
    std::iter::repeat_n(byte, 64).collect()
}

fn publication(
    repository_id: RepositoryId,
    epoch: char,
    previous_epoch: Option<GraphPublicationEpoch>,
) -> CurrentPublication {
    CurrentPublication {
        repository_id,
        epoch: GraphPublicationEpoch::parse(digest(epoch)).unwrap(),
        graph_content_version: GraphContentVersion::parse(digest('a')).unwrap(),
        physical_graph_key: format!("immutable-{epoch}"),
        artifact_version: ArtifactVersion::parse(digest('b')).unwrap(),
        graph_content_manifest_digest: ManifestDigest::parse(digest('c')).unwrap(),
        validation_digest: ValidationDigest::parse(digest('d')).unwrap(),
        previous_epoch,
    }
}

#[tokio::test]
async fn ladybug_publication_pointer_survives_reconnect_and_ignores_orphan_epoch() {
    let root = tempfile::tempdir().expect("publication root");
    let repository_id = RepositoryId::parse(digest('1')).unwrap();
    let first = publication(repository_id.clone(), '2', None);
    let store = LadybugPublicationStore::connect(root.path()).unwrap();
    assert_eq!(
        store
            .compare_and_swap(
                &repository_id,
                None,
                &first,
                PublisherFencingToken::new(1).unwrap(),
            )
            .await
            .unwrap(),
        PublicationCasResult::Published
    );
    drop(store);

    let second = publication(repository_id.clone(), '3', Some(first.epoch.clone()));
    let epoch_path = root
        .path()
        .join(".publications")
        .join(repository_id.as_str())
        .join(format!("{}.json", second.epoch.as_str()));
    std::fs::write(&epoch_path, serde_json::to_vec(&second).unwrap()).unwrap();

    let reconnected = LadybugPublicationStore::connect(root.path()).unwrap();
    assert_eq!(
        reconnected.current(&repository_id).await.unwrap(),
        Some(first.clone()),
        "an orphan epoch written before pointer CAS must not become current"
    );
    assert_eq!(
        reconnected
            .compare_and_swap(
                &repository_id,
                Some(&first.epoch),
                &second,
                PublisherFencingToken::new(2).unwrap(),
            )
            .await
            .unwrap(),
        PublicationCasResult::Published
    );
    assert_eq!(
        reconnected.current(&repository_id).await.unwrap(),
        Some(second)
    );
}

/// Ladybug-specific: a FAILED load must leave `CURRENT` — and this store's
/// own reads — on the previous good version (the flip happens only after a
/// successful load + checkpoint).
#[tokio::test(flavor = "multi_thread")]
async fn failed_load_keeps_previous_version_live() {
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

#[tokio::test(flavor = "multi_thread")]
async fn copy_round_trips_late_nested_call_sites_and_quoted_fields() {
    const EMPTY_CALL_ROWS: usize = 301;

    let root = tempfile::tempdir().expect("COPY regression root");
    let store = LadybugStore::connect(&root.path().to_string_lossy(), "copy-dialect")
        .expect("connect Ladybug");
    let source_id = NodeId::new("Method:Fixture#source/0");
    let source = Node {
        id: source_id.clone(),
        kind: NodeKind::Method,
        name: "source, \"quoted\"\nsecond line".into(),
        qualified_name: Some("Fixture#source/0".into()),
        file: "fixtures/quoted,source.rs".into(),
        range: Range::default(),
        props: Some(serde_json::json!({
            "text": "comma, quote \" and newline\nare preserved"
        })),
    };
    let mut nodes = vec![source.clone()];
    let mut edges = Vec::new();

    for index in 0..EMPTY_CALL_ROWS {
        let target_id = NodeId::new(format!("Method:Fixture#empty{index}/0"));
        nodes.push(Node {
            id: target_id.clone(),
            kind: NodeKind::Method,
            name: format!("empty{index}"),
            qualified_name: None,
            file: "fixtures/empty.rs".into(),
            range: Range::default(),
            props: None,
        });
        edges.push(Edge::new(
            source_id.clone(),
            target_id,
            EdgeKind::Calls,
            1.0,
            String::new(),
        ));
    }

    let nested_id = NodeId::new("Method:Fixture#nested/3");
    nodes.push(Node {
        id: nested_id.clone(),
        kind: NodeKind::Method,
        name: "nested".into(),
        qualified_name: None,
        file: "fixtures/nested.rs".into(),
        range: Range::default(),
        props: None,
    });
    let call_sites = serde_json::json!([{
        "range": {
            "start_line": 17,
            "start_col": 4,
            "end_line": 18,
            "end_col": 9
        },
        "args": ["comma,inside", "quote \"inside\"", "line one\nline two"]
    }]);
    edges.push(Edge {
        src: source_id.clone(),
        dst: nested_id.clone(),
        kind: EdgeKind::Calls,
        confidence: 0.75,
        reason: "late, \"nested\"\nrelationship".into(),
        props: Some(serde_json::json!({"call_sites": call_sites.clone()})),
    });

    let artifacts_dir = root.path().join("artifacts");
    let artifacts = GraphArtifacts::write(
        &artifacts_dir,
        VersionId::new("copy-dialect-v1"),
        &nodes,
        &edges,
    )
    .expect("write graph artifacts");
    let stats = store.bulk_load(&artifacts).await.expect("COPY graph");
    assert_eq!(stats.nodes, (EMPTY_CALL_ROWS + 2) as u64);
    assert_eq!(stats.edges, (EMPTY_CALL_ROWS + 1) as u64);

    let read_source = store
        .get_node(&source_id)
        .await
        .expect("read source")
        .expect("source exists");
    assert_eq!(read_source.name, source.name);
    assert_eq!(read_source.props, source.props);

    let nested = store
        .batched_transitions(
            std::slice::from_ref(&source_id),
            &TransitionQuery {
                direction: Direction::Downstream,
                edge_kinds: vec![EdgeKind::Calls],
                page_limit: EMPTY_CALL_ROWS + 2,
                after: None,
            },
        )
        .await
        .expect("read CALLS edges")
        .transitions
        .into_iter()
        .find(|transition| transition.edge.dst == nested_id)
        .expect("nested CALLS edge")
        .edge;
    assert_eq!(nested.reason, "late, \"nested\"\nrelationship");
    assert_eq!(
        nested.props,
        Some(serde_json::json!({"call_sites": call_sites}))
    );
}
