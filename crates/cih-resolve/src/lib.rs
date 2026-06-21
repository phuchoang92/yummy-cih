//! Phase 4.1/4.2 — resolution indexes and reference-site edge emission.
//!
//! Loads the Phase-3 `ParsedFile` IR for a scope and builds read-only, cross-file
//! indexes the emit passes query: a def/type registry, per-file import tables,
//! heritage adjacency, and a precedence-ordered scope-binding lookup that turns a
//! receiver name into a resolved FQCN. The public [`resolve_edges`] entrypoint runs
//! the Phase 4.2 pass order and emits graph edges.

use cih_core::{Edge, Node, NodeId, ParsedFile, Range};
use serde::{Deserialize, Serialize};

pub mod common;
pub mod complexity;
pub mod constant_propagation;
pub mod lang;
pub mod similarity;

mod contracts;
pub mod db_access;
pub mod di_xml;
mod emit;
mod index;
pub mod integration_xml;
pub mod reports;
mod types;

pub use common::emit::EdgeEmitter;
pub use common::index::CommonIndex;
pub use complexity::propagate_loop_depths;
pub use constant_propagation::build_java_constant_resolver;
pub use similarity::emit_similar_to_edges;
pub use contracts::resolve_contract_edges;
pub use db_access::emit_db_access;
pub use di_xml::{extract_di_xml, DiXmlOutput};
pub use integration_xml::{extract_integration_xml, IntegrationXmlOutput};
pub use lang::{
    java::JavaResolver, python::PythonResolver, typescript::TypeScriptResolver, ResolverRegistry,
};
pub use reports::write_unresolved_reports;

/// Per-site diagnostic record for a reference that could not be resolved.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UnresolvedRef {
    pub file: String,
    pub kind: String,
    pub name: String,
    pub receiver: Option<String>,
    pub arity: Option<u16>,
    pub in_fqcn: String,
    pub in_callable: NodeId,
    pub range: Range,
    /// Reason taxonomy: receiver_type_unknown | receiver_external | member_not_found |
    /// ctor_type_unknown | type_ref_unknown | heritage_type_unknown |
    /// free_call_unresolved | field_not_found | callresult_return_type_unknown
    pub reason: String,
    pub resolved_receiver_type: Option<String>,
    pub external_fqcn: Option<String>,
}

/// Result of turning unresolved reference sites into graph edges.
#[derive(Clone, Debug, Default)]
pub struct ResolveOutput {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// Reference/import sites that could not be resolved to an in-scope node.
    pub skipped: u64,
    /// Qualified external types discovered while trying to resolve calls/ctors.
    pub unresolved_external_fqcns: Vec<String>,
    /// Per-site diagnostic records for all unresolved references.
    pub unresolved_refs: Vec<UnresolvedRef>,
}

/// Options for the configurable resolve entrypoint.
pub struct ResolveOptions<'a> {
    pub repo_root: Option<&'a std::path::Path>,
    pub enable_xml_integrations: bool,
    /// Optional constant resolver to enrich CALLS edge call-site args (Gap 3/4).
    /// Pass `None` to use the no-op `NullConstantResolver`.
    pub constant_resolver: Option<Box<dyn cih_lang::constant_resolver::ConstantResolver>>,
}

/// Build the default registry with Java, TypeScript, and Python resolvers.
pub fn default_registry() -> ResolverRegistry {
    let mut r = ResolverRegistry::new();
    r.register(JavaResolver);
    r.register(TypeScriptResolver);
    r.register(PythonResolver);
    r
}

/// Backward-compatible entrypoint (uses old ResolveIndex for tests).
/// Run Phase 4.2 over all parsed files: receiver-bound calls, free calls,
/// remaining references, import edges, then heritage edges.
pub fn resolve_edges(parsed: &[ParsedFile]) -> ResolveOutput {
    let index = index::ResolveIndex::build(parsed);
    emit::EdgeEmitter::new(parsed, index).run()
}

/// Configurable entrypoint used by the engine.
pub fn resolve_with_registry(
    parsed: &[ParsedFile],
    registry: &ResolverRegistry,
    options: ResolveOptions<'_>,
) -> ResolveOutput {
    let index = CommonIndex::build(parsed, registry);
    let emitter = EdgeEmitter::new(parsed, index, registry);
    let emitter = if let Some(cr) = options.constant_resolver {
        emitter.with_constant_resolver_boxed(cr)
    } else {
        emitter
    };
    emitter.run()
}

#[cfg(test)]
mod tests;
