use std::sync::{Arc, Mutex};

use anyhow::Result;
use cih_core::GraphArtifacts;
use cih_falkor::FalkorStore;
use cih_graph_store::{GraphStore, LoadObserver, LoadStats};

use crate::ui::PhaseProgress;

/// Outcome of the FalkorDB load step — distinguishes a deliberate skip from a failure.
pub enum LoadOutcome {
    Loaded(LoadStats),
    Reused,
    Skipped,
    Failed(String),
}

impl LoadOutcome {
    pub fn status(&self) -> &'static str {
        match self {
            LoadOutcome::Loaded(_) => "loaded",
            LoadOutcome::Reused => "reused",
            LoadOutcome::Skipped => "skipped",
            LoadOutcome::Failed(_) => "failed",
        }
    }

    pub fn stats(&self) -> Option<&LoadStats> {
        match self {
            LoadOutcome::Loaded(stats) => Some(stats),
            _ => None,
        }
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            LoadOutcome::Failed(reason) => Some(reason.as_str()),
            _ => None,
        }
    }
}

/// Load multiple artifact sets into one staging graph, then publish atomically.
/// Callers supply artifacts in the order they should be merged (analyze first, community second).
pub fn load_many_to_falkor(
    url: &str,
    graph_key: &str,
    artifact_sets: &[&GraphArtifacts],
) -> Result<cih_graph_store::LoadStats> {
    crate::runtime::block_on(async {
        let staging_key = format!("{graph_key}-staging");
        let store = FalkorStore::connect(url, &staging_key)
            .map_err(|e| anyhow::anyhow!("FalkorDB connect: {e}"))?;
        // No `ensure_schema` here: the first `bulk_load` into the freshly-dropped
        // (unused) staging key takes the `GRAPH.BULK` fast path, which requires an
        // unused key and creates the indexes itself afterward. The Cypher fallback
        // (used for later sets, e.g. community) creates the id index idempotently.
        let _ = store.drop_graph().await;

        let mut total_nodes = 0u64;
        let mut total_edges = 0u64;
        for artifacts in artifact_sets {
            let stats = store
                .bulk_load(artifacts)
                .await
                .map_err(|e| anyhow::anyhow!("FalkorDB bulk_load: {e}"))?;
            total_nodes += stats.nodes;
            total_edges += stats.edges;
        }

        store
            .publish_to(graph_key)
            .await
            .map_err(|e| anyhow::anyhow!("FalkorDB publish: {e}"))?;
        if let Err(err) = store.drop_graph().await {
            tracing::warn!(
                graph = staging_key,
                error = %err,
                "failed to drop FalkorDB staging graph"
            );
        }
        Ok(cih_graph_store::LoadStats {
            nodes: total_nodes,
            edges: total_edges,
        })
    })
}

pub fn load_to_falkor(
    url: &str,
    graph_key: &str,
    artifacts: &GraphArtifacts,
) -> Result<cih_graph_store::LoadStats> {
    load_many_to_falkor(url, graph_key, &[artifacts])
}

/// Drives the shared `PhaseProgress` UI: each bulk-load milestone finishes the
/// current phase and spins up the next. Owns an `Arc<Mutex<PhaseProgress>>` (the
/// same pattern as `wiki/flow_enrich.rs`) so the orchestration below and these
/// callbacks share one bar. Assumes a single artifact set — `nodes_loaded` etc.
/// each fire once — which is why only `analyze`/`resolve` use it (multi-set
/// `discover` keeps the plain `load_many_to_falkor`).
struct PhaseObserver {
    ui: Arc<Mutex<PhaseProgress>>,
}

impl LoadObserver for PhaseObserver {
    fn nodes_loaded(&self, count: u64) {
        let mut ui = self.ui.lock().expect("UI progress mutex poisoned");
        ui.finish_with(format!("{} loaded", crate::ui::fmt_count(count as usize)));
        ui.spin("Loading edges");
    }

    fn edges_loaded(&self, count: u64) {
        let mut ui = self.ui.lock().expect("UI progress mutex poisoned");
        ui.finish_with(format!("{} loaded", crate::ui::fmt_count(count as usize)));
        ui.spin("Building indexes");
    }

    fn indexes_built(&self) {
        self.ui
            .lock()
            .expect("UI progress mutex poisoned")
            .finish_with("done");
    }
}

/// Single-set FalkorDB load that renders live multi-phase progress
/// (Connecting → nodes → edges → indexes → Publishing). Mirrors
/// [`load_many_to_falkor`]'s staging-then-publish flow but interleaves phase
/// transitions; `nodes/edges/indexes` are driven by [`PhaseObserver`] from inside
/// the bulk insert. Pass `quiet = true` (e.g. under `--json`) to hide the UI; a
/// non-TTY hides it automatically. Never holds the UI mutex across an `.await`.
pub fn load_to_falkor_with_progress(
    url: &str,
    graph_key: &str,
    artifacts: &GraphArtifacts,
    quiet: bool,
) -> Result<cih_graph_store::LoadStats> {
    let ui = Arc::new(Mutex::new(PhaseProgress::new()));
    if quiet {
        ui.lock().expect("UI progress mutex poisoned").hide();
    }
    let observer = PhaseObserver { ui: ui.clone() };

    crate::runtime::block_on(async {
        let staging_key = format!("{graph_key}-staging");

        ui.lock()
            .expect("UI progress mutex poisoned")
            .spin("Connecting to FalkorDB");
        let store = FalkorStore::connect(url, &staging_key)
            .map_err(|e| anyhow::anyhow!("FalkorDB connect: {e}"))?;
        // Fresh staging key → bulk_load takes the GRAPH.BULK fast path (see db docs).
        let _ = store.drop_graph().await;
        ui.lock()
            .expect("UI progress mutex poisoned")
            .finish_with("staging ready");

        // Observer finishes "Loading nodes"/"Loading edges" and spins the next.
        ui.lock()
            .expect("UI progress mutex poisoned")
            .spin("Loading nodes");
        let stats = store
            .bulk_load_observed(artifacts, &observer)
            .await
            .map_err(|e| anyhow::anyhow!("FalkorDB bulk_load: {e}"))?;

        ui.lock()
            .expect("UI progress mutex poisoned")
            .spin("Publishing");
        store
            .publish_to(graph_key)
            .await
            .map_err(|e| anyhow::anyhow!("FalkorDB publish: {e}"))?;
        ui.lock()
            .expect("UI progress mutex poisoned")
            .finish_with(format!("→ {graph_key}"));

        if let Err(err) = store.drop_graph().await {
            tracing::warn!(
                graph = staging_key,
                error = %err,
                "failed to drop FalkorDB staging graph"
            );
        }
        Ok(stats)
    })
}
