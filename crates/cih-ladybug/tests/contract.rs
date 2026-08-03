//! The backend-neutral `GraphStore` contract suite against LadybugDB —
//! **hermetic**: embedded DB over a tempdir, no external service, runs in the
//! default `cargo test --workspace`. The first backend whose contract run
//! needs no docker.

use std::sync::Arc;

use cih_core::RepositoryId;
use cih_graph_store::publication::{
    ArtifactVersion, CurrentPublication, GraphContentVersion, GraphPublicationEpoch,
    GraphPublicationStore, ManifestDigest, PublicationCasResult, PublisherFencingToken,
    ValidationDigest,
};
use cih_graph_store::GraphStore;
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
