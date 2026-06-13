//! `GraphStore` — the storage-agnostic port. The engine and MCP tools talk
//! ONLY to this trait; each graph DB is an adapter (cih-falkor, future
//! cih-neptune, cih-postgres). Methods are DOMAIN operations, not raw queries,
//! so swapping backends never touches callers.
//!
//! Neptune / Neo4j / FalkorDB all speak openCypher → they share a
//! `CypherGraphStore` impl (parameterized by a driver + dialect); only the
//! Postgres-CTE adapter is fully separate.

use async_trait::async_trait;
use cih_core::{Edge, EdgeKind, GraphArtifacts, GraphDelta, Node, NodeId, VersionId};
use serde::{Deserialize, Serialize};

#[derive(thiserror::Error, Debug)]
pub enum GraphStoreError {
    #[error("graph backend error: {0}")]
    Backend(String),
    #[error("node not found: {0}")]
    NotFound(String),
    #[error("not implemented for this backend: {0}")]
    Unimplemented(&'static str),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, GraphStoreError>;

/// Traversal direction for impact / neighbor queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// callers — who depends on this symbol (blast radius).
    Upstream,
    /// callees — what this symbol depends on.
    Downstream,
    Both,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoadStats {
    pub nodes: u64,
    pub edges: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImpactNode {
    pub id: NodeId,
    pub depth: u32,
    pub via: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Impact {
    pub root: NodeId,
    pub direction: Direction,
    pub affected: Vec<ImpactNode>,
    /// none | low | medium | high | critical (derived from fan-out).
    pub risk: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Path {
    pub nodes: Vec<NodeId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Subgraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SymbolContext {
    pub node: Node,
    pub callers: Vec<Node>,
    pub callees: Vec<Node>,
    pub processes: Vec<String>,
}

/// The pluggable storage port. MCP tools map 1:1 onto the read methods.
#[async_trait]
pub trait GraphStore: Send + Sync {
    // ---- writes / lifecycle ----
    async fn ensure_schema(&self) -> Result<()>;
    async fn bulk_load(&self, artifacts: &GraphArtifacts) -> Result<LoadStats>;
    async fn upsert_incremental(&self, delta: &GraphDelta) -> Result<()>;
    async fn swap_version(&self, version: &VersionId) -> Result<()>;

    // ---- reads (domain queries) ----
    async fn get_node(&self, id: &NodeId) -> Result<Option<Node>>;
    async fn neighbors(&self, id: &NodeId, dir: Direction, kinds: &[EdgeKind]) -> Result<Vec<Edge>>;
    async fn impact(&self, id: &NodeId, dir: Direction, max_depth: u32) -> Result<Impact>;
    async fn call_chain(&self, from: &NodeId, to: &NodeId, max_depth: u32) -> Result<Vec<Path>>;
    async fn subgraph(&self, seeds: &[NodeId], radius: u32) -> Result<Subgraph>;
    async fn context(&self, id: &NodeId) -> Result<SymbolContext>;
}

/// Bulk loading is a SEPARATE port — mechanisms differ wildly across backends
/// (Neptune S3 loader, Neo4j admin import, FalkorDB bulk tool, Postgres COPY).
#[async_trait]
pub trait BulkLoader: Send + Sync {
    async fn load(&self, artifacts: &GraphArtifacts) -> Result<LoadStats>;
}

/// Derive a coarse risk label from upstream fan-out — shared helper so every
/// adapter reports risk consistently.
pub fn risk_from_fanout(affected: usize) -> &'static str {
    match affected {
        0 => "none",
        1..=5 => "low",
        6..=20 => "medium",
        21..=75 => "high",
        _ => "critical",
    }
}
