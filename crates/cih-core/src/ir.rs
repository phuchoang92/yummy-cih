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
    /// Receiver-name → raw-type bindings (params, locals, fields, `var` inference,
    /// patterns, aliases) scoped to their enclosing callable. Phase 4 uses these,
    /// precedence-ordered, to resolve a receiver's type. Raw (unresolved) names.
    pub type_bindings: Vec<TypeBinding>,
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
    /// Parameter types for methods/constructors, ordered, raw (simple/unresolved)
    /// names — empty for non-callables. Phase 4 uses these for overload narrowing.
    #[serde(default)]
    pub param_types: Vec<String>,
    /// Return type for methods, raw name (`None` for `void`/non-methods).
    #[serde(default)]
    pub return_type: Option<String>,
    /// Declared type for fields, raw name (`None` for non-fields).
    #[serde(default)]
    pub declared_type: Option<String>,
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
    /// Signature of the enclosing callable (`fqcn#name/arity`); kept for debugging
    /// and as a fallback. Prefer [`ReferenceSite::in_callable`] for the edge source.
    pub in_fqcn: String,
    /// Graph node id of the enclosing callable — the edge SOURCE Phase 4 attributes
    /// this reference to. A real [`NodeId`] (not the `in_fqcn` string), so resolved
    /// `CALLS`/`ACCESSES` edges never dangle.
    #[serde(default = "unknown_callable_id")]
    pub in_callable: NodeId,
}

fn unknown_callable_id() -> NodeId {
    NodeId::new("Method:<unknown>")
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

/// A receiver-name → raw-type binding scoped to its enclosing callable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeBinding {
    /// Bound identifier — the receiver name (`service`, `u`, …).
    pub name: String,
    /// Raw (unresolved) type name as written (`OwnerService`, `List`); or, for
    /// `var` call-result inference, the invoked method name whose return type the
    /// resolver must follow.
    pub raw_type: String,
    /// How the binding was introduced — drives Phase 4 resolution precedence and
    /// whether `raw_type` is a type or a method/alias to chase.
    pub kind: BindingKind,
    /// Signature of the enclosing callable (`fqcn#name/arity`), or the type FQCN for
    /// a field binding — the lexical scope this binding lives in.
    pub in_fqcn: String,
    pub range: Range,
}

/// Origin of a [`TypeBinding`] — determines resolution precedence (nearest
/// param/local beats a field) and how `raw_type` is interpreted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BindingKind {
    /// Method/constructor formal parameter (`void f(User u)`).
    Param,
    /// Local variable with an explicit type (`User u = …`).
    Local,
    /// Class field (`private User user;`).
    Field,
    /// `var x = svc.get();` — `raw_type` is the invoked method name to follow.
    CallResult,
    /// `var y = x;` — `raw_type` is another bound name to alias.
    Alias,
    /// Pattern binding (`if (o instanceof User u)`, `case User u ->`).
    Pattern,
    /// Method return-type binding.
    Return,
}
