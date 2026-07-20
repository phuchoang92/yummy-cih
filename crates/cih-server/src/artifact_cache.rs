//! Process-wide cache of parsed graph artifacts (`nodes.jsonl` + `edges.jsonl`),
//! invalidated on the nodes.jsonl and edges.jsonl mtimes, with single-flight coalescing of
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
    edges_mtime: Option<SystemTime>,
}

/// Cross-call cache of parsed artifacts, keyed by artifacts dir, with
/// single-flight loads (concurrent first-time loads for the same dir coalesce).
#[derive(Clone, Default)]
pub(crate) struct ArtifactCache {
    cache: Arc<MtimeCache<ArtifactBundle>>,
}

impl ArtifactCache {
    /// Server-lifetime cache: bounded by the shared artifact retention policy
    /// (entry cap + idle TTL). `Default` stays unlimited for tests.
    pub(crate) fn new() -> Self {
        Self {
            cache: Arc::new(MtimeCache::with_limits(
                crate::mtime_cache::CacheLimits::artifact_from_env(),
            )),
        }
    }

    /// Parsed nodes+edges for `artifacts_dir`, reused across calls until
    /// nodes.jsonl or edges.jsonl changes on disk; concurrent first-time loads for the same
    /// dir coalesce into one. On a miss the read+parse happens here, so callers
    /// on the async runtime should still guard it with `spawn_blocking` where
    /// they already did.
    pub(crate) fn bundle(&self, artifacts_dir: &str) -> std::io::Result<Arc<ArtifactBundle>> {
        let nodes_mtime = std::fs::metadata(PathBuf::from(artifacts_dir).join("nodes.jsonl"))
            .and_then(|meta| meta.modified())
            .ok();
        let edges_mtime = std::fs::metadata(PathBuf::from(artifacts_dir).join("edges.jsonl"))
            .and_then(|meta| meta.modified())
            .ok();
        self.cache.get_or_load(
            artifacts_dir,
            |bundle| {
                bundle.nodes_mtime == nodes_mtime
                    && bundle.edges_mtime == edges_mtime
                    && nodes_mtime.is_some()
                    && edges_mtime.is_some()
            },
            || {
                let nodes = load_artifact_nodes(artifacts_dir)?;
                let edges = load_artifact_edges(artifacts_dir)?;
                Ok(ArtifactBundle {
                    nodes,
                    edges,
                    nodes_mtime,
                    edges_mtime,
                })
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{EdgeKind, NodeId, NodeKind, Range};

    fn write_fixture(dir: &std::path::Path, edges: &[Edge]) {
        let node = Node {
            id: NodeId::new("Method:a.B#c/0"),
            kind: NodeKind::Method,
            name: "c".into(),
            qualified_name: None,
            file: "src/a.rs".into(),
            range: Range::default(),
            props: None,
        };
        std::fs::write(
            dir.join("nodes.jsonl"),
            format!("{}\n", serde_json::to_string(&node).unwrap()),
        )
        .unwrap();
        let raw = edges
            .iter()
            .map(|edge| serde_json::to_string(edge).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join("edges.jsonl"), raw).unwrap();
    }

    #[test]
    fn edge_only_change_invalidates_raw_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let edge = Edge::new(
            NodeId::new("Method:a.B#c/0"),
            NodeId::new("Method:a.B#d/0"),
            EdgeKind::Calls,
            1.0,
            "test".into(),
        );
        write_fixture(dir.path(), std::slice::from_ref(&edge));
        let cache = ArtifactCache::new();
        let key = dir.path().to_str().unwrap();
        let first = cache.bundle(key).unwrap();
        assert_eq!(first.edges.len(), 1);
        assert!(Arc::ptr_eq(&first, &cache.bundle(key).unwrap()));

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.path().join("edges.jsonl"), "").unwrap();
        let reloaded = cache.bundle(key).unwrap();
        assert!(!Arc::ptr_eq(&first, &reloaded));
        assert!(reloaded.edges.is_empty());
    }
}
