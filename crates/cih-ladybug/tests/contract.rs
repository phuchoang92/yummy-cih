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
