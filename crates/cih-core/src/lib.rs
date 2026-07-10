//! Core domain types shared across the CIH engine and graph-store adapters.
//!
//! Milestone 1 keeps `NodeId` as a string newtype (the stable, qualified node
//! id). A later milestone can intern ids to `u32` behind this type without
//! touching adapters.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

mod artifacts; // JSONL read/write helpers on GraphArtifacts (Phase 2)
pub mod entrypoints;
pub mod group;
pub mod ir;
pub mod registry;
pub mod repo_map;

pub use entrypoints::{
    build_calls_digraph, score_all_entry_points, score_entry_points, to_legacy_pairs,
    EntrypointKind, EntrypointRegistry, ScoredEntrypoint,
};
pub use group::{
    contracts_path, group_dir, normalize_contract_path, ContractMatch, ContractMatchKind,
    GroupEntry, GroupRegistry,
};
pub use ir::{
    BindingKind, BodyFingerprint, CallSiteRecord, ComplexityRecord, ContractKind, ContractSite,
    ImportBinding, ImportBindingKind, MessagingFramework, ParsedFile, ParsedUnit, RawImport,
    RefKind, ReferenceSite, SqlConstant, SqlExecutionSite, StringConstant, StructuralProfile,
    SymbolDef, TypeBinding,
};
pub use registry::{git_changed_files, git_head, now_rfc3339, Registry, RegistryEntry, RegistryStats};
pub use repo_map::{
    auto_detect_architecture, ArchitectureHint, BuildSystem, JarInfo, ModuleInfo, RepoMap,
};

/// Stable, unique node identifier (e.g. `Method:com.acme.UserService#save`).
///
/// The canonical format is `Kind:fully.qualified.name` — construct via the
/// `*_id()` helpers below (or `NodeId::new` when the string is already in
/// canonical form, e.g. read back from a store).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(String);

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
///
/// Graph labels are the variant names verbatim (strum's default); they are
/// stored in FalkorDB, so renaming a variant is a breaking schema change.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
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
    KafkaTopic,
    ExternalEndpoint,
    DbQuery,
    DbTable,
    IntegrationRoute,
    MessageDestination,
    Other,
}

impl NodeKind {
    pub fn label(&self) -> &'static str {
        (*self).into()
    }

    /// Unknown labels map to `Other` (labels read back from a store may
    /// come from a newer schema).
    pub fn from_label(label: &str) -> Self {
        label.parse().unwrap_or(NodeKind::Other)
    }
}

/// Origin framework for an HTTP route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteSource {
    SpringMvc,
    JaxRs,
    Express,
    NestJs,
    Flask,
    FastApi,
    Django,
}

pub fn function_id(fqn: &str, name: &str, arity: u16) -> NodeId {
    NodeId::new(format!("Function:{fqn}#{name}/{arity}"))
}

pub fn file_id(rel: &str) -> NodeId {
    NodeId::new(format!("File:{rel}"))
}

pub fn folder_id(rel: &str) -> NodeId {
    NodeId::new(format!("Folder:{rel}"))
}

pub fn type_id(kind: NodeKind, fqcn: &str) -> NodeId {
    let prefix = match kind {
        NodeKind::Class => "Class",
        NodeKind::Interface => "Interface",
        NodeKind::Enum => "Enum",
        NodeKind::Record => "Record",
        NodeKind::Annotation => "Annotation",
        _ => panic!("type_id only supports type node kinds"),
    };
    NodeId::new(format!("{prefix}:{fqcn}"))
}

pub fn method_id(fqcn: &str, name: &str, arity: u16) -> NodeId {
    NodeId::new(format!("Method:{fqcn}#{name}/{arity}"))
}

pub fn constructor_id(fqcn: &str, arity: u16) -> NodeId {
    NodeId::new(format!("Constructor:{fqcn}#<init>/{arity}"))
}

pub fn field_id(fqcn: &str, name: &str) -> NodeId {
    NodeId::new(format!("Field:{fqcn}#{name}"))
}

pub fn community_id(idx: usize) -> NodeId {
    NodeId::new(format!("Community:{idx}"))
}

pub fn process_id(entry_slug: &str, hash: &str) -> NodeId {
    NodeId::new(format!("Process:{entry_slug}-{hash}"))
}

pub fn kafka_topic_id(topic: &str) -> NodeId {
    NodeId::new(format!("KafkaTopic:{topic}"))
}

pub fn external_endpoint_id(method: &str, url_template: &str) -> NodeId {
    NodeId::new(format!(
        "ExternalEndpoint:{}:{}",
        method.to_ascii_uppercase(),
        url_template
    ))
}

pub fn db_query_const_id(owner_fqcn: &str, const_name: &str) -> NodeId {
    NodeId::new(format!("DbQuery:{owner_fqcn}#{const_name}"))
}

pub fn db_query_inline_id(file: &str, line: u32, col: u32) -> NodeId {
    NodeId::new(format!("DbQuery:{file}:{line}:{col}"))
}

pub fn db_table_id(table: &str) -> NodeId {
    NodeId::new(format!("DbTable:{}", table.to_ascii_uppercase()))
}

pub fn integration_route_id(source: &str, route_id: &str) -> NodeId {
    NodeId::new(format!("IntegrationRoute:{source}:{route_id}"))
}

pub fn message_destination_id(dest_type: &str, name: &str) -> NodeId {
    NodeId::new(format!("MessageDestination:{dest_type}:{name}"))
}

/// Edge types (mirrors `gitnexus-shared` `RelationshipType`, trimmed for v1).
///
/// Cypher labels are SCREAMING_SNAKE_CASE of the variant name (except `Other`
/// → `REL`); they are stored in FalkorDB, so renaming a variant is a breaking
/// schema change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, strum::IntoStaticStr)]
#[strum(serialize_all = "SCREAMING_SNAKE_CASE")]
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
    PublishesEvent,
    ListensTo,
    ExternalCall,
    Tests,
    ExecutesQuery,
    ReadsTable,
    WritesTable,
    IntegrationLink,
    SimilarTo,
    /// Inter-procedural taint flow from an entry-point method to a sink method.
    /// Emitted by `cih-taint` Phase 0. Props: `hops`, `sink_category`, `hop_count`.
    TaintFlow,
    #[strum(serialize = "REL")]
    Other,
}

impl EdgeKind {
    /// openCypher relationship label used by the Cypher adapters.
    pub fn cypher_label(&self) -> &'static str {
        (*self).into()
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
    /// Optional edge properties. CALLS edges use: `{"call_sites": [{range, args}]}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub props: Option<serde_json::Value>,
}

impl Edge {
    /// Constructor that fills in the new optional `props` field with None.
    pub fn new(src: NodeId, dst: NodeId, kind: EdgeKind, confidence: f32, reason: String) -> Self {
        Self {
            src,
            dst,
            kind,
            confidence,
            reason,
            props: None,
        }
    }
}

fn default_confidence() -> f32 {
    1.0
}

/// Monotonic publish version for atomic store swaps.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionId(String);

impl VersionId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for VersionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Canonical bulk-load artifact the engine always emits; each `BulkLoader`
/// transforms it into its backend's required format (S3 CSV, COPY, etc.).
#[derive(Clone, Debug)]
pub struct GraphArtifacts {
    pub nodes_path: PathBuf,
    pub edges_path: PathBuf,
    pub version: VersionId,
}

/// Manifest written at the head of a `.cih` bundle archive (Gap 5).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CihBundleManifest {
    /// Bundle format version; always 1 for this implementation.
    pub bundle_version: u8,
    /// CIH engine semver string (e.g. `"0.1.0"`).
    pub cih_version: String,
    /// Short repo name (last path component of root).
    pub repo_name: String,
    /// Absolute root path at export time.
    pub root_path: String,
    /// ISO 8601 timestamp of export.
    pub indexed_at: String,
    /// Graph artifact version hash.
    pub artifact_version: String,
    /// Whether community nodes/edges are included.
    pub has_community: bool,
    /// Number of indexed source files.
    pub file_count: usize,
}

/// Incremental change set for a re-index of a few files.
#[derive(Clone, Debug, Default)]
pub struct GraphDelta {
    pub changed_files: Vec<String>,
    pub removed_files: Vec<String>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}
