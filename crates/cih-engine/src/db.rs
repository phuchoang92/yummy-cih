use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use cih_core::GraphArtifacts;
use cih_graph_store::{GraphStore, LoadObserver, LoadStats};
use cih_store_factory::{connect_store, StoreOptions};

use crate::ui::PhaseProgress;

/// Outcome of the graph load step — distinguishes a deliberate skip from a failure.
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

/// Connect to the staging graph for `graph_key` on the configured backend.
/// No query limit: engine loads are sequential.
fn connect_staging(backend: &str, url: &str, staging_key: &str) -> Result<Arc<dyn GraphStore>> {
    connect_store(backend, url, staging_key, &StoreOptions::default())
}

/// Return a process- and attempt-unique staging key.
///
/// The old fixed `<graph>-staging` key allowed concurrent publishers to drop or
/// mutate each other's in-progress graph. The timestamp distinguishes process
/// restarts, the process ID distinguishes concurrent engine processes, and the
/// atomic sequence distinguishes attempts created in the same nanosecond.
fn staging_attempt_key(graph_key: &str) -> String {
    static ATTEMPT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let sequence = ATTEMPT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "{graph_key}-staging-{:x}-{nanos:x}-{sequence:x}",
        std::process::id()
    )
}

fn progress_spin(ui: Option<&Arc<Mutex<PhaseProgress>>>, message: &str) {
    if let Some(ui) = ui {
        ui.lock().expect("UI progress mutex poisoned").spin(message);
    }
}

fn progress_finish(ui: Option<&Arc<Mutex<PhaseProgress>>>, message: impl Into<String>) {
    if let Some(ui) = ui {
        ui.lock()
            .expect("UI progress mutex poisoned")
            .finish_with(message.into());
    }
}

/// Load a complete base artifact plus zero or more overlays into an owned
/// staging graph, then replace the destination only after every load succeeds.
///
/// Cleanup always targets the unique staging attempt. A failed load or publish
/// therefore cannot delete another publisher's work or mutate the destination
/// through a cleanup path.
async fn load_replacement_inner(
    backend: &str,
    url: &str,
    graph_key: &str,
    base: &GraphArtifacts,
    overlays: &[&GraphArtifacts],
    observer: Option<&dyn LoadObserver>,
    ui: Option<&Arc<Mutex<PhaseProgress>>>,
) -> Result<LoadStats> {
    let staging_key = staging_attempt_key(graph_key);
    tracing::info!(
        destination_graph = graph_key,
        staging_graph = staging_key,
        base_version = %base.version,
        overlay_count = overlays.len(),
        "starting owned graph publication attempt"
    );

    progress_spin(ui, "Connecting to graph store");
    let store = connect_staging(backend, url, &staging_key)?;

    let result: Result<LoadStats> = async {
        // The key is unique, so it should not exist. An idempotent delete both
        // verifies that assumption and prevents an astronomically unlikely key
        // collision from turning the first load into an upsert over stale data.
        store
            .drop_graph()
            .await
            .map_err(|error| anyhow::anyhow!("prepare staging graph: {error}"))?;
        progress_finish(ui, "staging ready");

        progress_spin(ui, "Loading nodes");
        let base_stats = match observer {
            Some(observer) => store
                .bulk_load_observed(base, observer)
                .await
                .map_err(|error| anyhow::anyhow!("graph base bulk_load: {error}"))?,
            None => store
                .bulk_load(base)
                .await
                .map_err(|error| anyhow::anyhow!("graph base bulk_load: {error}"))?,
        };
        let mut total_nodes = base_stats.nodes;
        let mut total_edges = base_stats.edges;

        for overlay in overlays {
            let stats = store
                .bulk_load(overlay)
                .await
                .map_err(|error| anyhow::anyhow!("graph overlay bulk_load: {error}"))?;
            total_nodes = total_nodes.saturating_add(stats.nodes);
            total_edges = total_edges.saturating_add(stats.edges);
        }

        progress_spin(ui, "Publishing");
        store
            .publish_to(graph_key)
            .await
            .map_err(|error| anyhow::anyhow!("graph publish: {error}"))?;
        progress_finish(ui, format!("→ {graph_key}"));

        Ok(LoadStats {
            nodes: total_nodes,
            edges: total_edges,
        })
    }
    .await;

    // `publish_to` consumes/moves the staging graph on both current adapters,
    // making this a harmless idempotent no-op after success. On failure it
    // removes only this attempt's partial graph.
    let cleanup = store.drop_graph().await;
    match (result, cleanup) {
        (Ok(stats), Ok(())) => Ok(stats),
        (Ok(stats), Err(error)) => {
            tracing::warn!(
                staging_graph = staging_key,
                error = %error,
                "published graph but failed to clean the owned staging attempt"
            );
            Ok(stats)
        }
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => {
            tracing::warn!(
                staging_graph = staging_key,
                error = %cleanup_error,
                "failed to clean the owned staging attempt after publication failure"
            );
            Err(anyhow::anyhow!(
                "{error:#}; additionally failed to clean staging graph '{staging_key}': {cleanup_error}"
            ))
        }
    }
}

/// Publish a replacement graph assembled from one complete base artifact and
/// zero or more ordered overlays.
pub fn load_replacement(
    backend: &str,
    url: &str,
    graph_key: &str,
    base: &GraphArtifacts,
    overlays: &[&GraphArtifacts],
) -> Result<LoadStats> {
    crate::runtime::block_on(load_replacement_inner(
        backend, url, graph_key, base, overlays, None, None,
    ))
}

/// Compatibility wrapper for the existing discover call path. The first set is
/// always the complete analyze artifact; remaining sets are overlays.
pub fn load_many(
    backend: &str,
    url: &str,
    graph_key: &str,
    artifact_sets: &[&GraphArtifacts],
) -> Result<cih_graph_store::LoadStats> {
    let (base, overlays) = artifact_sets
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("replacement publication requires a base artifact"))?;
    load_replacement(backend, url, graph_key, base, overlays)
}

/// Drives the shared `PhaseProgress` UI: each bulk-load milestone finishes the
/// current phase and spins up the next. Owns an `Arc<Mutex<PhaseProgress>>` (the
/// same pattern as `wiki/flow_enrich.rs`) so the orchestration below and these
/// callbacks share one bar. Assumes a single artifact set — `nodes_loaded` etc.
/// each fire once — which is why only `analyze`/`resolve` use it (multi-set
/// `discover` keeps the plain `load_many`).
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

/// Single-set graph load that renders live multi-phase progress
/// (Connecting → nodes → edges → indexes → Publishing). Mirrors
/// [`load_many`]'s staging-then-publish flow but interleaves phase
/// transitions; `nodes/edges/indexes` are driven by [`PhaseObserver`] from inside
/// the bulk insert (adapters without phase events degrade to a plain load).
/// Pass `quiet = true` (e.g. under `--json`) to hide the UI; a
/// non-TTY hides it automatically. Never holds the UI mutex across an `.await`.
pub fn load_with_progress(
    backend: &str,
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

    crate::runtime::block_on(load_replacement_inner(
        backend,
        url,
        graph_key,
        artifacts,
        &[],
        Some(&observer),
        Some(&ui),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn staging_attempt_keys_are_scoped_and_unique() {
        let keys: HashSet<_> = (0..10_000)
            .map(|_| staging_attempt_key("repo-graph"))
            .collect();

        assert_eq!(keys.len(), 10_000);
        assert!(keys
            .iter()
            .all(|key| key.starts_with("repo-graph-staging-")));
    }

    #[test]
    fn load_many_requires_an_explicit_base_artifact() {
        let error = load_many("unused", "unused", "repo-graph", &[]).unwrap_err();
        assert!(
            error.to_string().contains("requires a base artifact"),
            "{error:#}"
        );
    }

    #[cfg(feature = "ladybug")]
    fn node(id: &str) -> cih_core::Node {
        cih_core::Node {
            id: cih_core::NodeId::new(id),
            kind: cih_core::NodeKind::Method,
            name: id.to_string(),
            qualified_name: None,
            file: "Fixture.java".to_string(),
            range: cih_core::Range::default(),
            props: None,
        }
    }

    #[cfg(feature = "ladybug")]
    #[test]
    fn failed_overlay_load_cleans_owned_staging_and_preserves_live_graph() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store_root = temp.path().join("ladybug");
        let store_root = store_root.to_string_lossy().into_owned();
        let artifacts_root = temp.path().join("artifacts");

        let old = GraphArtifacts::write(
            &artifacts_root.join("old"),
            cih_core::VersionId::new("old"),
            &[node("Method:Old#run/0")],
            &[],
        )
        .expect("old artifacts");
        load_replacement("ladybug", &store_root, "live", &old, &[])
            .expect("publish initial live graph");
        let current_path = temp.path().join("ladybug/live/CURRENT");
        let current_before = std::fs::read_to_string(&current_path).expect("initial CURRENT");

        let replacement = GraphArtifacts::write(
            &artifacts_root.join("replacement"),
            cih_core::VersionId::new("replacement"),
            &[node("Method:New#run/0")],
            &[],
        )
        .expect("replacement artifacts");
        let broken_dir = artifacts_root.join("broken-overlay");
        std::fs::create_dir_all(&broken_dir).expect("broken overlay dir");
        std::fs::write(broken_dir.join("nodes.jsonl"), b"not-json\n").expect("broken nodes");
        std::fs::write(broken_dir.join("edges.jsonl"), b"").expect("empty edges");
        let broken_overlay = GraphArtifacts {
            nodes_path: broken_dir.join("nodes.jsonl"),
            edges_path: broken_dir.join("edges.jsonl"),
            version: cih_core::VersionId::new("broken-overlay"),
        };

        let error = load_replacement(
            "ladybug",
            &store_root,
            "live",
            &replacement,
            &[&broken_overlay],
        )
        .expect_err("broken overlay must abort publication");
        assert!(error.to_string().contains("overlay bulk_load"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(&current_path).expect("CURRENT survives"),
            current_before,
            "failed replacement must leave the live pointer unchanged"
        );

        let abandoned_staging: Vec<_> = std::fs::read_dir(temp.path().join("ladybug"))
            .expect("ladybug root")
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("live-staging-")
            })
            .collect();
        assert!(
            abandoned_staging.is_empty(),
            "owned staging graph must be cleaned after failure: {abandoned_staging:?}"
        );
    }
}
