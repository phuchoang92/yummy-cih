//! Per-file parse IR (Phase 3 output). The structural parts are emitted directly
//! as graph `Node`/`Edge`; `imports` + `reference_sites` are collected here
//! UNRESOLVED and consumed by Phase 4 (scope resolution) to emit
//! `CALLS`/`EXTENDS`/`ACCESSES`/… edges.

use crate::{NodeId, NodeKind, Range};
use serde::{Deserialize, Serialize};

/// Everything the parser extracts from one source file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedFile {
    /// Repo-relative path of the source file.
    pub file: String,
    /// Declared Java package (`None` = default package).
    pub package: Option<String>,
    /// Type / method / constructor / field definitions declared in this file.
    pub defs: Vec<SymbolDef>,
    /// Raw (unresolved) import statements; resolved in Phase 4.
    pub imports: Vec<RawImport>,
    /// Unresolved usage sites (calls, field access, heritage); resolved in Phase 4.
    pub reference_sites: Vec<ReferenceSite>,
}

/// A declared symbol — a type, method, constructor, or field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolDef {
    /// Graph node id, built via the locked id scheme
    /// (`cih_core::{type_id, method_id, constructor_id, field_id}`).
    pub id: NodeId,
    pub kind: NodeKind,
    /// The FQCN this id is built from: the type's **own** FQCN for a type, or the
    /// **enclosing type's** FQCN for a method/constructor/field member.
    pub fqcn: String,
    /// Simple (unqualified) name.
    pub name: String,
    /// Enclosing type's node id for members; `None` for top-level types.
    pub owner: Option<NodeId>,
    pub range: Range,
    /// Source modifiers (`public`, `static`, `abstract`, …).
    pub modifiers: Vec<String>,
}

/// A raw import statement, pre-resolution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawImport {
    /// Imported path as written, e.g. `java.util.List` or `com.acme.util.*`.
    pub raw: String,
    /// `import static …`.
    pub is_static: bool,
    /// Wildcard import (`…*`).
    pub is_wildcard: bool,
    pub range: Range,
}

/// A usage site (call / field access / heritage) before resolution. Phase 4 turns
/// each into a graph edge — or drops it if the target is out of scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceSite {
    /// Referenced name (method / field / type simple name).
    pub name: String,
    /// Explicit receiver text for member calls (`service` in `service.save()`).
    pub receiver: Option<String>,
    pub kind: RefKind,
    /// Argument count for calls; `None` for non-call references.
    pub arity: Option<u16>,
    pub range: Range,
    /// FQCN of the enclosing callable — the edge SOURCE Phase 4 attributes this to.
    pub in_fqcn: String,
}

/// What a [`ReferenceSite`] represents → the graph-edge kind emitted after resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefKind {
    Call,
    FieldRead,
    FieldWrite,
    Ctor,
    Extends,
    Implements,
    TypeRef,
}
