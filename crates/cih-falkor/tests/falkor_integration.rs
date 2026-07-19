//! Live-FalkorDB integration test — runs the backend-neutral contract suite
//! (`cih_graph_store::contract`) against a real FalkorDB, proving the adapter
//! honors the `GraphStore` port: load round-trips, incremental upsert,
//! staging→publish→drop, and observed-load phase events.
//!
//! `#[ignore]`d so `cargo test --workspace` stays hermetic. Run against a
//! FalkorDB reachable at `FALKOR_URL` (default `redis://127.0.0.1:6380`):
//!
//! ```text
//! cargo test -p cih-falkor --test falkor_integration -- --ignored
//! ```

use std::sync::Arc;

use cih_falkor::FalkorStore;
use cih_graph_store::GraphStore;

fn falkor_url() -> String {
    std::env::var("FALKOR_URL").unwrap_or_else(|_| "redis://127.0.0.1:6380".to_string())
}

#[tokio::test]
#[ignore = "requires a live FalkorDB (FALKOR_URL, default redis://127.0.0.1:6380); run with --ignored"]
async fn falkor_passes_the_graph_store_contract() {
    let url = falkor_url();
    cih_graph_store::contract::run_contract_suite(|graph_key: &str| {
        let store: Arc<dyn GraphStore> = Arc::new(FalkorStore::connect(&url, graph_key)?);
        Ok(store)
    })
    .await
    .expect("contract suite infrastructure");
}
