//! Core domain types shared across the CIH engine and graph-store adapters.
//!
//! Milestone 1 keeps `NodeId` as a string newtype (the stable, qualified node
//! id). A later milestone can intern ids to `u32` behind this type without
//! touching adapters.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

mod artifacts; // JSONL read/write helpers on GraphArtifacts (Phase 2)
pub mod ir;
pub mod repo_map;

pub use ir::{ParsedFile, RawImport, RefKind, ReferenceSite, SymbolDef};
pub use repo_map::{BuildSystem, JarInfo, ModuleInfo, RepoMap, SpringSignal};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_helpers_use_locked_scheme() {
        assert_eq!(
            file_id("src/main/java/App.java").as_str(),
            "File:src/main/java/App.java"
        );
        assert_eq!(folder_id("src/main/java").as_str(), "Folder:src/main/java");
        assert_eq!(
            type_id(NodeKind::Class, "com.acme.Outer.Inner").as_str(),
            "Class:com.acme.Outer.Inner"
        );
        assert_eq!(
            type_id(NodeKind::Interface, "com.acme.Service").as_str(),
            "Interface:com.acme.Service"
        );
        assert_eq!(
            method_id("com.acme.Outer.Inner", "save", 2).as_str(),
            "Method:com.acme.Outer.Inner#save/2"
        );
        assert_eq!(
            constructor_id("com.acme.Outer.Inner", 1).as_str(),
            "Constructor:com.acme.Outer.Inner#<init>/1"
        );
        assert_eq!(
            field_id("com.acme.Outer.Inner", "name").as_str(),
            "Field:com.acme.Outer.Inner#name"
        );
    }

    #[test]
    fn repo_map_round_trips_json() {
        let repo_map = RepoMap {
            root: "/repo".into(),
            build_system: BuildSystem::Maven,
            total_java_files: 3,
            total_loc: 120,
            modules: vec![ModuleInfo {
                name: "app".into(),
                rel_path: ".".into(),
                build_file: Some("pom.xml".into()),
                java_files: 3,
                loc: 120,
                packages: vec!["com.acme".into()],
                spring: SpringSignal {
                    services: 1,
                    controllers: 1,
                    ..SpringSignal::default()
                },
                depends_on: vec!["core".into()],
            }],
            jars: vec![JarInfo {
                path: "lib/example.jar".into(),
                group_id: Some("com.acme".into()),
                artifact: Some("example".into()),
                is_own: true,
                classes: 12,
            }],
            decompiled_dirs: vec![".workspace-dependencies".into()],
        };

        let encoded = serde_json::to_string(&repo_map).unwrap();
        let decoded: RepoMap = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, repo_map);
    }

    #[test]
    fn parsed_file_round_trips_json() {
        let parsed = ParsedFile {
            file: "src/main/java/com/acme/UserService.java".into(),
            package: Some("com.acme".into()),
            defs: vec![SymbolDef {
                id: method_id("com.acme.UserService", "save", 1),
                kind: NodeKind::Method,
                fqcn: "com.acme.UserService".into(),
                name: "save".into(),
                owner: Some(type_id(NodeKind::Class, "com.acme.UserService")),
                range: Range {
                    start_line: 10,
                    start_col: 4,
                    end_line: 12,
                    end_col: 5,
                },
                modifiers: vec!["public".into()],
            }],
            imports: vec![RawImport {
                raw: "java.util.List".into(),
                is_static: false,
                is_wildcard: false,
                range: Range {
                    start_line: 3,
                    start_col: 0,
                    end_line: 3,
                    end_col: 22,
                },
            }],
            reference_sites: vec![ReferenceSite {
                name: "findById".into(),
                receiver: Some("repository".into()),
                kind: RefKind::Call,
                arity: Some(1),
                range: Range {
                    start_line: 11,
                    start_col: 16,
                    end_line: 11,
                    end_col: 24,
                },
                in_fqcn: "com.acme.UserService#save/1".into(),
            }],
        };

        let encoded = serde_json::to_string(&parsed).unwrap();
        let decoded: ParsedFile = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, parsed);
    }
}
