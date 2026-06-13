//! Core domain types shared across the CIH engine and graph-store adapters.
//!
//! Milestone 1 keeps `NodeId` as a string newtype (the stable, qualified node
//! id). A later milestone can intern ids to `u32` behind this type without
//! touching adapters.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

mod artifacts; // JSONL read/write helpers on GraphArtifacts (Phase 2)

/// Stable, unique node identifier (e.g. `Method:com.acme.UserService#save`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl NodeId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Node labels (mirrors `gitnexus-shared` `NodeLabel`, trimmed for Java/Spring v1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    File,
    Folder,
    Class,
    Interface,
    Enum,
    Record,
    Annotation,
    Method,
    Function,
    Constructor,
    Field,
    Route,
    Community,
    Process,
    Other,
}

/// Edge types (mirrors `gitnexus-shared` `RelationshipType`, trimmed for v1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EdgeKind {
    Contains,
    Calls,
    Extends,
    Implements,
    HasMethod,
    HasField,
    Imports,
    Accesses,
    Uses,
    MethodOverrides,
    MethodImplements,
    MemberOf,
    StepInProcess,
    HandlesRoute,
    Other,
}

impl EdgeKind {
    /// openCypher relationship label used by the Cypher adapters.
    pub fn cypher_label(&self) -> &'static str {
        match self {
            EdgeKind::Contains => "CONTAINS",
            EdgeKind::Calls => "CALLS",
            EdgeKind::Extends => "EXTENDS",
            EdgeKind::Implements => "IMPLEMENTS",
            EdgeKind::HasMethod => "HAS_METHOD",
            EdgeKind::HasField => "HAS_FIELD",
            EdgeKind::Imports => "IMPORTS",
            EdgeKind::Accesses => "ACCESSES",
            EdgeKind::Uses => "USES",
            EdgeKind::MethodOverrides => "METHOD_OVERRIDES",
            EdgeKind::MethodImplements => "METHOD_IMPLEMENTS",
            EdgeKind::MemberOf => "MEMBER_OF",
            EdgeKind::StepInProcess => "STEP_IN_PROCESS",
            EdgeKind::HandlesRoute => "HANDLES_ROUTE",
            EdgeKind::Other => "REL",
        }
    }
}

/// 1-based lines, 0-based columns (matches the engine's `ParsedFile` ranges).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
    pub file: String,
    #[serde(default)]
    pub range: Range,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub props: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Edge {
    pub src: NodeId,
    pub dst: NodeId,
    pub kind: EdgeKind,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    #[serde(default)]
    pub reason: String,
}

fn default_confidence() -> f32 {
    1.0
}

/// Monotonic publish version for atomic store swaps.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionId(pub String);

/// Canonical bulk-load artifact the engine always emits; each `BulkLoader`
/// transforms it into its backend's required format (S3 CSV, COPY, etc.).
#[derive(Clone, Debug)]
pub struct GraphArtifacts {
    pub nodes_path: PathBuf,
    pub edges_path: PathBuf,
    pub version: VersionId,
}

/// Incremental change set for a re-index of a few files.
#[derive(Clone, Debug, Default)]
pub struct GraphDelta {
    pub changed_files: Vec<String>,
    pub removed_files: Vec<String>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}
