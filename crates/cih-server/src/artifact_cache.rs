//! Process-wide cache of parsed graph artifacts (`nodes.jsonl` + `edges.jsonl`),
//! invalidated on the nodes.jsonl mtime — the same `get_or_load` pattern as
//! [`crate::xflow::XflowState`], but keeping the raw file-ordered `Vec<Node>` /
//! `Vec<Edge>` that `taint_paths` and `shape_check` consume (rather than the
//! id-keyed adjacency `ArtifactGraph` builds). Preserves node/edge ordering, so
//! callers that iterate in file order get byte-identical results.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use cih_core::{Edge, Node};

use crate::utils::{load_artifact_edges, load_artifact_nodes};

pub(crate) struct ArtifactBundle {
    pub(crate) nodes: Vec<Node>,
    pub(crate) edges: Vec<Edge>,
    nodes_mtime: Option<SystemTime>,
}

/// Cross-call cache of parsed artifacts, keyed by artifacts dir.
#[derive(Clone, Default)]
pub(crate) struct ArtifactCache {
    cache: Arc<RwLock<HashMap<PathBuf, Arc<ArtifactBundle>>>>,
}

impl ArtifactCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Parsed nodes+edges for `artifacts_dir`, reused across calls until
    /// nodes.jsonl changes on disk. On a miss the read+parse happens here, so
    /// callers on the async runtime should still guard it with `spawn_blocking`
    /// where they already did.
    pub(crate) fn bundle(&self, artifacts_dir: &str) -> std::io::Result<Arc<ArtifactBundle>> {
        let key = PathBuf::from(artifacts_dir);
        let mtime = std::fs::metadata(key.join("nodes.jsonl"))
            .and_then(|meta| meta.modified())
            .ok();
        if let Some(cached) = self.cache.read().expect("artifact cache lock").get(&key) {
            if cached.nodes_mtime == mtime && mtime.is_some() {
                return Ok(cached.clone());
            }
        }
        let nodes = load_artifact_nodes(artifacts_dir)?;
        let edges = load_artifact_edges(artifacts_dir)?;
        let bundle = Arc::new(ArtifactBundle {
            nodes,
            edges,
            nodes_mtime: mtime,
        });
        self.cache
            .write()
            .expect("artifact cache lock")
            .insert(key, bundle.clone());
        Ok(bundle)
    }
}
