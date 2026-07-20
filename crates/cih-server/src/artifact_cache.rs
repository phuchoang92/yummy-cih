//! Shared, process-wide snapshots of parsed graph artifacts.
//!
//! One snapshot owns the file-ordered node/edge arrays used by taint, shape,
//! and cross-repo flow. Adjacency indexes are lazy and store positions only,
//! avoiding the second full-node representation previously owned by xflow.

use std::collections::HashMap;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use cih_core::{Edge, Node};

use crate::mtime_cache::MtimeCache;
use crate::utils::{load_artifact_edges, load_artifact_nodes};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    modified: Option<SystemTime>,
    len: Option<u64>,
}

impl FileIdentity {
    fn probe(path: &Path) -> Self {
        match std::fs::metadata(path) {
            Ok(metadata) => Self {
                modified: metadata.modified().ok(),
                len: Some(metadata.len()),
            },
            Err(_) => Self {
                modified: None,
                len: None,
            },
        }
    }

    fn exists(self) -> bool {
        self.modified.is_some() && self.len.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ArtifactFreshness {
    nodes: FileIdentity,
    edges: FileIdentity,
}

impl ArtifactFreshness {
    fn probe(dir: &Path) -> Self {
        Self {
            nodes: FileIdentity::probe(&dir.join("nodes.jsonl")),
            edges: FileIdentity::probe(&dir.join("edges.jsonl")),
        }
    }

    fn is_complete(self) -> bool {
        self.nodes.exists() && self.edges.exists()
    }
}

pub(crate) struct ArtifactIndexes {
    pub(crate) node_by_id: HashMap<String, usize>,
    pub(crate) outgoing_edges: HashMap<String, Vec<usize>>,
    pub(crate) incoming_edges: HashMap<String, Vec<usize>>,
}

impl ArtifactIndexes {
    fn build(nodes: &[Node], edges: &[Edge]) -> Self {
        let node_by_id = nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.id.as_str().to_string(), index))
            .collect();
        let mut outgoing_edges: HashMap<String, Vec<usize>> = HashMap::new();
        let mut incoming_edges: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, edge) in edges.iter().enumerate() {
            outgoing_edges
                .entry(edge.src.as_str().to_string())
                .or_default()
                .push(index);
            incoming_edges
                .entry(edge.dst.as_str().to_string())
                .or_default()
                .push(index);
        }
        Self {
            node_by_id,
            outgoing_edges,
            incoming_edges,
        }
    }
}

pub(crate) struct ArtifactSnapshot {
    pub(crate) version: String,
    pub(crate) nodes: Arc<[Node]>,
    pub(crate) edges: Arc<[Edge]>,
    freshness: ArtifactFreshness,
    indexes: OnceLock<Arc<ArtifactIndexes>>,
}

impl ArtifactSnapshot {
    fn from_parts(
        version: String,
        nodes: Vec<Node>,
        edges: Vec<Edge>,
        freshness: ArtifactFreshness,
    ) -> Self {
        Self {
            version,
            nodes: nodes.into(),
            edges: edges.into(),
            freshness,
            indexes: OnceLock::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_memory(nodes: Vec<Node>, edges: Vec<Edge>) -> Self {
        Self::from_parts(
            "memory".to_string(),
            nodes,
            edges,
            ArtifactFreshness {
                nodes: FileIdentity {
                    modified: None,
                    len: None,
                },
                edges: FileIdentity {
                    modified: None,
                    len: None,
                },
            },
        )
    }

    /// Build positional indexes on first use. Production callers reach this
    /// only from operations already admitted to the heavy blocking runtime.
    pub(crate) fn indexes_blocking(&self) -> &Arc<ArtifactIndexes> {
        self.indexes
            .get_or_init(|| Arc::new(ArtifactIndexes::build(&self.nodes, &self.edges)))
    }

    pub(crate) fn estimated_weight_bytes(&self) -> usize {
        let base = size_of::<Self>()
            .saturating_add(self.version.capacity())
            .saturating_add(self.nodes.len().saturating_mul(size_of::<Node>()))
            .saturating_add(self.edges.len().saturating_mul(size_of::<Edge>()));
        let node_dynamic = self.nodes.iter().fold(0usize, |total, node| {
            total
                .saturating_add(node.id.as_str().len())
                .saturating_add(node.name.capacity())
                .saturating_add(node.qualified_name.as_ref().map_or(0, String::capacity))
                .saturating_add(node.file.capacity())
                .saturating_add(node.props.as_ref().map_or(0, json_weight))
        });
        let edge_dynamic = self.edges.iter().fold(0usize, |total, edge| {
            total
                .saturating_add(edge.src.as_str().len())
                .saturating_add(edge.dst.as_str().len())
                .saturating_add(edge.reason.capacity())
                .saturating_add(edge.props.as_ref().map_or(0, json_weight))
        });
        let node_index_estimate = self
            .nodes
            .iter()
            .map(|node| {
                node.id
                    .as_str()
                    .len()
                    .saturating_add(size_of::<(String, usize)>().saturating_mul(2))
            })
            .fold(0usize, usize::saturating_add);
        // Price adjacency conservatively as though every endpoint creates a
        // map key. Real graphs usually reuse endpoints, so this leaves room
        // for hash buckets and Vec over-allocation without scanning indexes.
        let adjacency_index_estimate = self
            .edges
            .iter()
            .map(|edge| {
                edge.src
                    .as_str()
                    .len()
                    .saturating_add(edge.dst.as_str().len())
                    .saturating_add(size_of::<(String, Vec<usize>)>().saturating_mul(4))
                    .saturating_add(size_of::<usize>().saturating_mul(2))
            })
            .fold(0usize, usize::saturating_add);
        base.saturating_add(node_dynamic)
            .saturating_add(edge_dynamic)
            .saturating_add(node_index_estimate)
            .saturating_add(adjacency_index_estimate)
    }

    #[cfg(test)]
    pub(crate) fn indexes_initialized(&self) -> bool {
        self.indexes.get().is_some()
    }
}

fn json_weight(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            size_of::<serde_json::Value>()
        }
        serde_json::Value::String(value) => {
            size_of::<serde_json::Value>().saturating_add(value.capacity())
        }
        serde_json::Value::Array(values) => values.iter().fold(
            size_of::<serde_json::Value>().saturating_add(
                values
                    .capacity()
                    .saturating_mul(size_of::<serde_json::Value>()),
            ),
            |total, value| total.saturating_add(json_weight(value)),
        ),
        serde_json::Value::Object(values) => {
            values
                .iter()
                .fold(size_of::<serde_json::Value>(), |total, (key, value)| {
                    total
                        .saturating_add(key.capacity())
                        .saturating_add(json_weight(value))
                })
        }
    }
}

/// Cross-call cache of parsed artifacts, keyed by artifacts dir, with
/// single-flight loads (concurrent first-time loads for the same dir coalesce).
#[derive(Clone, Default)]
pub(crate) struct ArtifactCache {
    cache: Arc<MtimeCache<ArtifactSnapshot>>,
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

    /// Parsed nodes+edges for `artifacts_dir`, reused until either file identity
    /// changes. Production callers invoke this inside the blocking runtime.
    pub(crate) fn snapshot_blocking(
        &self,
        artifacts_dir: &str,
    ) -> std::io::Result<Arc<ArtifactSnapshot>> {
        let dir = PathBuf::from(artifacts_dir)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(artifacts_dir));
        let key = dir.to_string_lossy().into_owned();
        let freshness = ArtifactFreshness::probe(&dir);
        self.cache.get_or_load_weighted(
            &key,
            |snapshot| snapshot.freshness == freshness && freshness.is_complete(),
            || {
                let nodes = load_artifact_nodes(&key)?;
                let edges = load_artifact_edges(&key)?;
                let version = dir
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "unversioned".to_string());
                Ok(ArtifactSnapshot::from_parts(
                    version, nodes, edges, freshness,
                ))
            },
            ArtifactSnapshot::estimated_weight_bytes,
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
    fn edge_only_change_invalidates_shared_snapshot() {
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
        let first = cache.snapshot_blocking(key).unwrap();
        assert_eq!(first.edges.len(), 1);
        assert_eq!(
            first.version,
            dir.path().file_name().unwrap().to_string_lossy()
        );
        assert!(!first.indexes_initialized());
        assert!(Arc::ptr_eq(&first, &cache.snapshot_blocking(key).unwrap()));

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.path().join("edges.jsonl"), "").unwrap();
        let reloaded = cache.snapshot_blocking(key).unwrap();
        assert!(!Arc::ptr_eq(&first, &reloaded));
        assert!(reloaded.edges.is_empty());
    }

    #[test]
    fn node_only_change_invalidates_shared_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path(), &[]);
        let cache = ArtifactCache::new();
        let key = dir.path().to_str().unwrap();
        let first = cache.snapshot_blocking(key).unwrap();

        let mut changed = first.nodes[0].clone();
        changed.name = "changed".into();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            dir.path().join("nodes.jsonl"),
            format!("{}\n", serde_json::to_string(&changed).unwrap()),
        )
        .unwrap();

        let reloaded = cache.snapshot_blocking(key).unwrap();
        assert!(!Arc::ptr_eq(&first, &reloaded));
        assert_eq!(reloaded.nodes[0].name, "changed");
    }

    #[test]
    fn concurrent_callers_share_one_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(dir.path(), &[]);
        let cache = ArtifactCache::default();
        let key = dir.path().to_string_lossy().into_owned();
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let handles = (0..16)
            .map(|_| {
                let cache = cache.clone();
                let key = key.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    cache.snapshot_blocking(&key).unwrap()
                })
            })
            .collect::<Vec<_>>();
        let snapshots = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();

        assert!(
            snapshots
                .iter()
                .all(|snapshot| Arc::ptr_eq(&snapshots[0], snapshot)),
            "concurrent callers must share one retained snapshot"
        );
    }

    #[test]
    fn indexes_are_lazy_and_reference_shared_arrays() {
        let dir = tempfile::tempdir().unwrap();
        let edge = Edge::new(
            NodeId::new("Method:a.B#c/0"),
            NodeId::new("Method:a.B#d/0"),
            EdgeKind::Calls,
            1.0,
            "test".into(),
        );
        write_fixture(dir.path(), std::slice::from_ref(&edge));
        let snapshot = ArtifactCache::new()
            .snapshot_blocking(dir.path().to_str().unwrap())
            .unwrap();
        assert!(!snapshot.indexes_initialized());
        let indexes = snapshot.indexes_blocking();
        assert!(snapshot.indexes_initialized());
        assert_eq!(indexes.node_by_id["Method:a.B#c/0"], 0);
        assert_eq!(indexes.outgoing_edges["Method:a.B#c/0"], vec![0]);
        assert_eq!(indexes.incoming_edges["Method:a.B#d/0"], vec![0]);
    }
}
