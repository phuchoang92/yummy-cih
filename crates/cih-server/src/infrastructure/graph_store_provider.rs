//! Graph-store construction and startup schema policy.

use std::sync::Arc;

use anyhow::Result;
use cih_graph_store::GraphStore;

use crate::config::{store_options, store_runtime_options, Config};

pub(crate) async fn build_store(cfg: &Config) -> Result<Arc<dyn GraphStore>> {
    // Construction is lazy. In particular, do not run DDL or block server
    // startup while Redis/FalkorDB is restoring a large dataset; `/health`
    // must come up and `/ready` will report the dependency state.
    let store = cih_store_factory::connect_store_with_runtime(
        &cfg.backend,
        &cfg.falkor_url,
        &cfg.graph_key,
        &store_options(cfg),
        &store_runtime_options(cfg),
    )?;
    Ok(store)
}
