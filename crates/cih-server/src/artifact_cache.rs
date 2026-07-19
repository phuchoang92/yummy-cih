//! Process-wide cache of parsed graph artifacts (`nodes.jsonl` + `edges.jsonl`),
//! invalidated on the nodes.jsonl mtime, with single-flight coalescing of
//! concurrent misses (see [`crate::mtime_cache`]). Keeps the raw file-ordered
//! `Vec<Node>` / `Vec<Edge>` that `taint_paths` and `shape_check` consume
//! (rather than the id-keyed adjacency `ArtifactGraph` builds). Preserves
//! node/edge ordering, so callers that iterate in file order get byte-identical
//! results.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use cih_core::{Edge, Node};

use crate::mtime_cache::MtimeCache;
use crate::utils::{load_artifact_edges, load_artifact_nodes};

pub(crate) struct ArtifactBundle {
    pub(crate) nodes: Vec<Node>,
    pub(crate) edges: Vec<Edge>,
    nodes_mtime: Option<SystemTime>,
}

/// Cross-call cache of parsed artifacts, keyed by artifacts dir, with
/// single-flight loads (concurrent first-time loads for the same dir coalesce).
#[derive(Clone, Default)]
pub(crate) struct ArtifactCache {
    cache: Arc<MtimeCache<ArtifactBundle>>,
}

impl ArtifactCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Parsed nodes+edges for `artifacts_dir`, reused across calls until
    /// nodes.jsonl changes on disk; concurrent first-time loads for the same
    /// dir coalesce into one. On a miss the read+parse happens here, so callers
    /// on the async runtime should still guard it with `spawn_blocking` where
    /// they already did.
    pub(crate) fn bundle(&self, artifacts_dir: &str) -> std::io::Result<Arc<ArtifactBundle>> {
        let mtime = std::fs::metadata(PathBuf::from(artifacts_dir).join("nodes.jsonl"))
            .and_then(|meta| meta.modified())
            .ok();
        self.cache.get_or_load(
            artifacts_dir,
            |bundle| bundle.nodes_mtime == mtime && mtime.is_some(),
            || {
                let nodes = load_artifact_nodes(artifacts_dir)?;
                let edges = load_artifact_edges(artifacts_dir)?;
                Ok(ArtifactBundle {
                    nodes,
                    edges,
                    nodes_mtime: mtime,
                })
            },
        )
    }
}
