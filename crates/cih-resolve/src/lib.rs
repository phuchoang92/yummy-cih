//! Phase 4.1/4.2 — resolution indexes and reference-site edge emission.
//!
//! Loads the Phase-3 `ParsedFile` IR for a scope and builds read-only, cross-file
//! indexes the emit passes query: a def/type registry, per-file import tables,
//! heritage adjacency, and a precedence-ordered scope-binding lookup that turns a
//! receiver name into a resolved FQCN. The public [`resolve_edges`] entrypoint runs
//! the Phase 4.2 pass order and emits graph edges.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use cih_core::{
    external_endpoint_id, file_id, kafka_topic_id, BindingKind, ContractKind, Edge, EdgeKind, Node,
    NodeId, NodeKind, ParsedFile, Range, RawImport, RefKind, ReferenceSite, SymbolDef, TypeBinding,
};
use serde::{Deserialize, Serialize};

pub mod reports;
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

/// Result of turning unresolved [`ReferenceSite`]s into graph edges.
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

/// Run Phase 4.2 over all parsed files: receiver-bound calls, free calls,
/// remaining references, import edges, then heritage edges.
pub fn resolve_edges(parsed: &[ParsedFile]) -> ResolveOutput {
    let index = ResolveIndex::build(parsed);
    EdgeEmitter::new(parsed, index).run()
}

/// Cross-file resolution index over a parsed scope.
#[derive(Debug, Default)]
pub(crate) struct ResolveIndex {
    /// type FQCN → its def.
    types_by_fqcn: HashMap<String, SymbolDef>,
    /// simple type name → all FQCNs that share it (for unique-name fallback).
    simple_to_fqcns: HashMap<String, Vec<String>>,
    /// type FQCN → the file it was declared in (for raw→FQCN via that file's imports).
    file_of_type: HashMap<String, String>,
    /// `(owner_fqcn, method/ctor name)` → overloads.
    methods: HashMap<(String, String), Vec<SymbolDef>>,
    /// `(owner_fqcn, field name)` → field def.
    fields: HashMap<(String, String), SymbolDef>,
    /// file path → its package + imports.
    files: HashMap<String, FileContext>,
    /// scope FQCN (callable signature for params/locals, type FQCN for fields) →
    /// the bindings declared in it.
    bindings: HashMap<String, Vec<TypeBinding>>,
    /// type FQCN → resolved super/interface FQCNs.
    supertypes: HashMap<String, Vec<String>>,
    /// interface/super FQCN → types that extend/implement it.
    implementors: HashMap<String, Vec<String>>,
    /// type FQCN → Spring stereotype ("service", "repository", "component", …).
    type_stereotypes: HashMap<String, String>,
}

#[derive(Debug, Default)]
struct FileContext {
    package: Option<String>,
    imports: Vec<RawImport>,
}

impl ResolveIndex {
    /// Build the index from all `ParsedFile`s in the scope.
    pub(crate) fn build(parsed: &[ParsedFile]) -> Self {
        let mut idx = ResolveIndex::default();

        // Pass 1: defs, members, files, bindings.
        for pf in parsed {
            idx.files.insert(
                pf.file.clone(),
                FileContext {
                    package: pf.package.clone(),
                    imports: pf.imports.clone(),
                },
            );
            for def in &pf.defs {
                if is_type_kind(def.kind) {
                    if let Some(s) = def.stereotype.as_deref() {
                        idx.type_stereotypes.insert(def.fqcn.clone(), s.to_string());
                    }
                    idx.types_by_fqcn.insert(def.fqcn.clone(), def.clone());
                    idx.simple_to_fqcns
                        .entry(simple_of(&def.fqcn))
                        .or_default()
                        .push(def.fqcn.clone());
                    idx.file_of_type.insert(def.fqcn.clone(), pf.file.clone());
                } else if matches!(def.kind, NodeKind::Method | NodeKind::Constructor) {
                    idx.methods
                        .entry((def.fqcn.clone(), def.name.clone()))
                        .or_default()
                        .push(def.clone());
                } else if def.kind == NodeKind::Field {
                    idx.fields
                        .insert((def.fqcn.clone(), def.name.clone()), def.clone());
                }
            }
            for tb in &pf.type_bindings {
                idx.bindings
                    .entry(tb.in_fqcn.clone())
                    .or_default()
                    .push(tb.clone());
            }
        }

        // Pass 2: heritage (needs the full type registry + per-file imports).
        for pf in parsed {
            for site in &pf.reference_sites {
                if !matches!(site.kind, RefKind::Extends | RefKind::Implements) {
                    continue;
                }
                // For heritage sites `in_fqcn` is the subtype's class FQCN.
                let resolved = idx
                    .resolve_type(&site.name, &pf.file)
                    .unwrap_or_else(|| site.name.clone());
                idx.supertypes
                    .entry(site.in_fqcn.clone())
                    .or_default()
                    .push(resolved.clone());
                idx.implementors
                    .entry(resolved)
                    .or_default()
                    .push(site.in_fqcn.clone());
            }
        }

        idx.dedup();
        idx
    }

    // --- raw → FQCN -------------------------------------------------------

    /// Resolve a raw (as-written) type name to a FQCN, using the imports +
    /// package of `file`: explicit import → same package → wildcard import →
    /// workspace-unique simple name. Already-qualified names pass through.
    pub(crate) fn resolve_type(&self, raw: &str, file: &str) -> Option<String> {
        let base = base_type_name(raw);
        if base.is_empty() {
            return None;
        }
        if base.contains('.') {
            return Some(base); // already qualified
        }
        if let Some(ctx) = self.files.get(file) {
            for imp in &ctx.imports {
                if !imp.is_wildcard && imp.raw.rsplit('.').next() == Some(base.as_str()) {
                    return Some(imp.raw.clone());
                }
            }
            if let Some(pkg) = &ctx.package {
                let cand = format!("{pkg}.{base}");
                if self.types_by_fqcn.contains_key(&cand) {
                    return Some(cand);
                }
            }
            for imp in &ctx.imports {
                if imp.is_wildcard {
                    let cand = format!("{}.{base}", imp.raw.trim_end_matches(".*"));
                    if self.types_by_fqcn.contains_key(&cand) {
                        return Some(cand);
                    }
                }
            }
        }
        match self.simple_to_fqcns.get(&base) {
            Some(fqcns) if fqcns.len() == 1 => Some(fqcns[0].clone()),
            _ => None,
        }
    }

    // --- member lookup cascade -------------------------------------------

    /// Find a member's node id on `owner_fqcn` directly (no hierarchy walk):
    /// exact-arity overload → any overload → field.
    pub(crate) fn find_member(
        &self,
        owner_fqcn: &str,
        name: &str,
        arity: Option<u16>,
    ) -> Option<NodeId> {
        let key = (owner_fqcn.to_string(), name.to_string());
        if let Some(overloads) = self.methods.get(&key) {
            if let Some(a) = arity {
                if let Some(def) = overloads.iter().find(|d| d.param_types.len() as u16 == a) {
                    return Some(def.id.clone());
                }
            }
            return overloads.first().map(|d| d.id.clone());
        }
        self.fields.get(&key).map(|d| d.id.clone())
    }

    pub(crate) fn find_constructor(&self, owner_fqcn: &str, arity: Option<u16>) -> Option<NodeId> {
        self.find_member(owner_fqcn, "<init>", arity)
    }

    /// Like [`find_member`], but walks `owner_fqcn` + its supertypes (BFS) — the
    /// inheritance/MRO-ish member resolution the receiver-bound pass needs.
    pub(crate) fn find_member_in_hierarchy(
        &self,
        owner_fqcn: &str,
        name: &str,
        arity: Option<u16>,
    ) -> Option<NodeId> {
        let mut seen = HashSet::new();
        let mut queue = vec![owner_fqcn.to_string()];
        while let Some(cur) = queue.pop() {
            if !seen.insert(cur.clone()) {
                continue;
            }
            if let Some(id) = self.find_member(&cur, name, arity) {
                return Some(id);
            }
            if let Some(supers) = self.supertypes.get(&cur) {
                queue.extend(supers.iter().cloned());
            }
        }
        None
    }

    pub(crate) fn find_field_in_hierarchy(&self, owner_fqcn: &str, name: &str) -> Option<NodeId> {
        let mut seen = HashSet::new();
        let mut queue = vec![owner_fqcn.to_string()];
        while let Some(cur) = queue.pop() {
            if !seen.insert(cur.clone()) {
                continue;
            }
            if let Some(def) = self.fields.get(&(cur.clone(), name.to_string())) {
                return Some(def.id.clone());
            }
            if let Some(supers) = self.supertypes.get(&cur) {
                queue.extend(supers.iter().cloned());
            }
        }
        None
    }

    pub(crate) fn member_return_type_in_hierarchy(
        &self,
        owner_fqcn: &str,
        name: &str,
        arity: Option<u16>,
    ) -> Option<String> {
        let mut seen = HashSet::new();
        let mut queue = vec![owner_fqcn.to_string()];
        while let Some(cur) = queue.pop() {
            if !seen.insert(cur.clone()) {
                continue;
            }
            if let Some(overloads) = self.methods.get(&(cur.clone(), name.to_string())) {
                let raw = match arity {
                    Some(a) => overloads
                        .iter()
                        .find(|d| d.param_types.len() as u16 == a)
                        .and_then(|d| d.return_type.as_ref()),
                    None => overloads.iter().find_map(|d| d.return_type.as_ref()),
                };
                if let Some(raw) = raw {
                    return self.resolve_in_type(raw, &cur);
                }
            }
            if let Some(supers) = self.supertypes.get(&cur) {
                queue.extend(supers.iter().cloned());
            }
        }
        None
    }

    // --- receiver typing (precedence-ordered) ----------------------------

    /// Resolve a receiver name used inside callable `in_fqcn` to a type FQCN.
    /// Precedence: nearest param/local (then alias/call-result chains) → enclosing
    /// class field (incl. inherited) → `this`/`super`.
    pub(crate) fn receiver_type(&self, in_fqcn: &str, receiver: &str) -> Option<String> {
        self.receiver_type_inner(in_fqcn, receiver, 0)
    }

    fn receiver_type_inner(&self, in_fqcn: &str, receiver: &str, depth: u8) -> Option<String> {
        if depth > 8 {
            return None; // alias cycle guard
        }
        let owner_class = class_of(in_fqcn);
        match receiver {
            "this" => return Some(owner_class.to_string()),
            "super" => {
                return self
                    .supertypes
                    .get(owner_class)
                    .and_then(|s| s.first())
                    .cloned()
            }
            _ => {}
        }

        // 1. param / local / pattern / alias / call-result in this callable.
        if let Some(bindings) = self.bindings.get(in_fqcn) {
            if let Some(tb) = pick_binding(bindings, receiver) {
                return self.resolve_binding(tb, in_fqcn, depth);
            }
        }

        // 2. field on the enclosing class or a supertype.
        self.field_type_in_hierarchy(owner_class, receiver)
    }

    fn resolve_binding(&self, tb: &TypeBinding, in_fqcn: &str, depth: u8) -> Option<String> {
        let owner_class = class_of(in_fqcn);
        match tb.kind {
            BindingKind::Param
            | BindingKind::Local
            | BindingKind::Pattern
            | BindingKind::Field
            | BindingKind::Return => self.resolve_in_type(&tb.raw_type, owner_class),
            // `var y = x;` — raw_type is another bound name; chase it.
            BindingKind::Alias => self.receiver_type_inner(in_fqcn, &tb.raw_type, depth + 1),
            // `var x = m(...);` — raw_type is the method name.
            // 1. Check the enclosing class hierarchy (self/free calls).
            // 2. Scan fields of the enclosing class for the method when step 1 fails
            //    (factory pattern: `var x = this.factory.create()`).
            BindingKind::CallResult => self
                .method_return_type_in_hierarchy(owner_class, &tb.raw_type)
                .or_else(|| self.callresult_via_field_types(owner_class, &tb.raw_type)),
        }
    }

    pub(crate) fn field_type_in_hierarchy(&self, owner_class: &str, name: &str) -> Option<String> {
        let mut seen = HashSet::new();
        let mut queue = vec![owner_class.to_string()];
        while let Some(cur) = queue.pop() {
            if !seen.insert(cur.clone()) {
                continue;
            }
            if let Some(field) = self.fields.get(&(cur.clone(), name.to_string())) {
                if let Some(raw) = &field.declared_type {
                    return self.resolve_in_type(raw, &cur);
                }
            }
            if let Some(supers) = self.supertypes.get(&cur) {
                queue.extend(supers.iter().cloned());
            }
        }
        None
    }

    fn method_return_type_in_hierarchy(&self, owner_class: &str, name: &str) -> Option<String> {
        let mut seen = HashSet::new();
        let mut queue = vec![owner_class.to_string()];
        while let Some(cur) = queue.pop() {
            if !seen.insert(cur.clone()) {
                continue;
            }
            if let Some(overloads) = self.methods.get(&(cur.clone(), name.to_string())) {
                if let Some(ret) = overloads.iter().find_map(|d| d.return_type.as_ref()) {
                    return self.resolve_in_type(ret, &cur);
                }
            }
            if let Some(supers) = self.supertypes.get(&cur) {
                queue.extend(supers.iter().cloned());
            }
        }
        None
    }

    /// Fallback for `CallResult` bindings: if `method_name` is absent from `owner_class`'s
    /// hierarchy, scan all declared fields of `owner_class`. If exactly one field's type has
    /// the method, return its return type (handles `var x = factory.create()` patterns).
    fn callresult_via_field_types(&self, owner_class: &str, method_name: &str) -> Option<String> {
        let candidates: Vec<String> = self
            .fields
            .iter()
            .filter(|((fqcn, _), _)| fqcn == owner_class)
            .filter_map(|(_, def)| {
                let raw = def.declared_type.as_ref()?;
                let field_type = self.resolve_in_type(raw, owner_class)?;
                self.method_return_type_in_hierarchy(&field_type, method_name)
            })
            .collect();
        if candidates.len() == 1 {
            candidates.into_iter().next()
        } else {
            None
        }
    }

    /// Resolve a raw type name against the file that declares `type_fqcn`.
    fn resolve_in_type(&self, raw: &str, type_fqcn: &str) -> Option<String> {
        match self.file_of_type.get(type_fqcn) {
            Some(file) => self.resolve_type(raw, file),
            None => self.resolve_type(raw, ""),
        }
    }

    // --- accessors (for 4.2 / 4.3) ---------------------------------------

    pub(crate) fn supertypes(&self, fqcn: &str) -> &[String] {
        self.supertypes.get(fqcn).map(Vec::as_slice).unwrap_or(&[])
    }

    #[cfg(test)]
    pub(crate) fn implementors(&self, fqcn: &str) -> &[String] {
        self.implementors
            .get(fqcn)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub(crate) fn is_known_type(&self, fqcn: &str) -> bool {
        self.types_by_fqcn.contains_key(fqcn)
    }

    pub(crate) fn type_node_id(&self, fqcn: &str) -> Option<NodeId> {
        self.types_by_fqcn.get(fqcn).map(|def| def.id.clone())
    }

    /// Every type FQCN in the scope (for MRO / whole-graph passes).
    pub(crate) fn type_fqcns(&self) -> impl Iterator<Item = &str> {
        self.types_by_fqcn.keys().map(String::as_str)
    }

    fn dedup(&mut self) {
        for v in self.simple_to_fqcns.values_mut() {
            v.sort();
            v.dedup();
        }
        // Preserve insertion order for supertypes: C3 linearization requires the
        // superclass to appear before interfaces (extends clause precedes implements).
        for v in self.supertypes.values_mut() {
            stable_dedup(v);
        }
        for v in self.implementors.values_mut() {
            v.sort();
            v.dedup();
        }
    }

    pub(crate) fn is_interface_type(&self, fqcn: &str) -> bool {
        self.types_by_fqcn
            .get(fqcn)
            .map(|def| matches!(def.kind, NodeKind::Interface | NodeKind::Annotation))
            .unwrap_or(false)
    }

    fn is_spring_bean(&self, fqcn: &str) -> bool {
        matches!(
            self.type_stereotypes.get(fqcn).map(String::as_str),
            Some("service" | "repository" | "component" | "controller" | "configuration")
        )
    }

    /// Returns the single `@Service`/`@Component`/`@Repository` implementor for an
    /// interface, or `None` when there are zero or multiple (ambiguous).
    fn di_impl(&self, interface_fqcn: &str) -> Option<String> {
        let impls = self.implementors.get(interface_fqcn)?;
        let beans: Vec<&String> = impls.iter().filter(|f| self.is_spring_bean(f)).collect();
        if beans.len() == 1 {
            Some(beans[0].clone())
        } else {
            None
        }
    }
}

struct EdgeEmitter<'a> {
    parsed: &'a [ParsedFile],
    index: ResolveIndex,
    handled: HashSet<(usize, usize)>,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    skipped: u64,
    unresolved_external_fqcns: BTreeSet<String>,
    unresolved_refs: Vec<UnresolvedRef>,
}

impl<'a> EdgeEmitter<'a> {
    fn new(parsed: &'a [ParsedFile], index: ResolveIndex) -> Self {
        Self {
            parsed,
            index,
            handled: HashSet::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
            skipped: 0,
            unresolved_external_fqcns: BTreeSet::new(),
            unresolved_refs: Vec::new(),
        }
    }

    fn push_unresolved(
        &mut self,
        pf: &ParsedFile,
        site: &ReferenceSite,
        reason: &str,
        resolved_receiver_type: Option<String>,
        external_fqcn: Option<String>,
    ) {
        self.skipped += 1;
        if let Some(ref ext) = external_fqcn {
            self.unresolved_external_fqcns.insert(ext.clone());
        }
        self.unresolved_refs.push(UnresolvedRef {
            file: pf.file.clone(),
            kind: format!("{:?}", site.kind),
            name: site.name.clone(),
            receiver: site.receiver.clone(),
            arity: site.arity,
            in_fqcn: site.in_fqcn.clone(),
            in_callable: site.in_callable.clone(),
            range: site.range,
            reason: reason.to_string(),
            resolved_receiver_type,
            external_fqcn,
        });
    }

    fn classify_unresolved_ref(
        &mut self,
        pf: &ParsedFile,
        site: &ReferenceSite,
    ) -> (&'static str, Option<String>, Option<String>) {
        match site.kind {
            RefKind::Call => {
                if let Some(recv) = site.receiver.as_deref() {
                    match self.resolve_receiver_expr_type(pf, site, recv) {
                        Some(rt) if rt.contains('.') && !self.index.is_known_type(&rt) => {
                            ("receiver_external", None, Some(rt))
                        }
                        Some(rt) => ("member_not_found", Some(rt), None),
                        None => ("receiver_type_unknown", None, None),
                    }
                } else {
                    ("free_call_unresolved", None, None)
                }
            }
            RefKind::Ctor => {
                let ext = self
                    .index
                    .resolve_type(&site.name, &pf.file)
                    .filter(|f| f.contains('.') && !self.index.is_known_type(f));
                ("ctor_type_unknown", None, ext)
            }
            RefKind::TypeRef => {
                let ext = self
                    .index
                    .resolve_type(&site.name, &pf.file)
                    .filter(|f| f.contains('.') && !self.index.is_known_type(f));
                ("type_ref_unknown", None, ext)
            }
            RefKind::FieldRead | RefKind::FieldWrite => {
                let recv_type = site
                    .receiver
                    .as_deref()
                    .and_then(|r| self.resolve_receiver_expr_type(pf, site, r));
                ("field_not_found", recv_type, None)
            }
            _ => ("unresolved", None, None),
        }
    }

    fn run(mut self) -> ResolveOutput {
        self.emit_receiver_bound_calls();
        self.emit_free_call_fallback();
        self.emit_references_via_lookup();
        self.emit_import_edges();
        self.emit_heritage_edges();
        self.emit_mro_edges();
        let (contract_nodes, contract_edges) = resolve_contract_edges(self.parsed);
        self.nodes.extend(contract_nodes);
        self.edges.extend(contract_edges);
        self.finish()
    }

    fn emit_receiver_bound_calls(&mut self) {
        let parsed = self.parsed.to_vec();
        for (file_idx, pf) in parsed.iter().enumerate() {
            for (site_idx, site) in pf.reference_sites.iter().enumerate() {
                if site.kind != RefKind::Call || site.receiver.is_none() {
                    continue;
                }
                if let Some((dst, confidence, reason)) = self.resolve_receiver_bound_call(pf, site)
                {
                    self.push_edge(
                        site.in_callable.clone(),
                        dst,
                        EdgeKind::Calls,
                        confidence,
                        reason,
                    );
                    self.handled.insert((file_idx, site_idx));
                }
            }
        }
    }

    fn emit_free_call_fallback(&mut self) {
        let parsed = self.parsed.to_vec();
        for (file_idx, pf) in parsed.iter().enumerate() {
            for (site_idx, site) in pf.reference_sites.iter().enumerate() {
                if self.handled.contains(&(file_idx, site_idx)) {
                    continue;
                }
                if site.kind != RefKind::Call || site.receiver.is_some() {
                    continue;
                }

                let owner = class_of(&site.in_fqcn);
                let target = self
                    .index
                    .find_member_in_hierarchy(owner, &site.name, site.arity)
                    .or_else(|| self.find_static_imported_member(pf, &site.name, site.arity));

                if let Some(dst) = target {
                    self.push_edge(
                        site.in_callable.clone(),
                        dst,
                        EdgeKind::Calls,
                        0.8,
                        "free-call-fallback".to_string(),
                    );
                    self.handled.insert((file_idx, site_idx));
                }
            }
        }
    }

    fn emit_references_via_lookup(&mut self) {
        let parsed = self.parsed.to_vec();
        for (file_idx, pf) in parsed.iter().enumerate() {
            for (site_idx, site) in pf.reference_sites.iter().enumerate() {
                if self.handled.contains(&(file_idx, site_idx)) {
                    continue;
                }
                let resolved = match site.kind {
                    // Pass 1 (receiver-bound) and pass 2 (free-call) already tried every
                    // Call site. Any that reach here were unresolvable; don't retry.
                    RefKind::Call => None,
                    RefKind::Ctor => self.resolve_constructor(pf, site).map(|dst| {
                        (
                            site.in_callable.clone(),
                            dst,
                            EdgeKind::Calls,
                            1.0,
                            "constructor".to_string(),
                        )
                    }),
                    RefKind::FieldRead | RefKind::FieldWrite => {
                        self.resolve_field_access(pf, site).map(|dst| {
                            (
                                site.in_callable.clone(),
                                dst,
                                EdgeKind::Accesses,
                                1.0,
                                match site.kind {
                                    RefKind::FieldRead => "field-read",
                                    _ => "field-write",
                                }
                                .to_string(),
                            )
                        })
                    }
                    RefKind::TypeRef => self.resolve_type_node(pf, &site.name).map(|dst| {
                        (
                            site.in_callable.clone(),
                            dst,
                            EdgeKind::Uses,
                            1.0,
                            "type-ref".to_string(),
                        )
                    }),
                    RefKind::Extends | RefKind::Implements => None,
                };

                if let Some((src, dst, kind, confidence, reason)) = resolved {
                    self.push_edge(src, dst, kind, confidence, reason);
                    self.handled.insert((file_idx, site_idx));
                } else if !matches!(site.kind, RefKind::Extends | RefKind::Implements) {
                    let (reason, recv_type, ext_fqcn) =
                        self.classify_unresolved_ref(pf, site);
                    self.push_unresolved(pf, site, reason, recv_type, ext_fqcn);
                }
            }
        }
    }

    fn emit_import_edges(&mut self) {
        let parsed = self.parsed.to_vec();
        for pf in &parsed {
            for import in &pf.imports {
                let Some(dst) = self.resolve_import_target(pf, import) else {
                    continue;
                };
                self.push_edge(
                    file_id(&pf.file),
                    dst,
                    EdgeKind::Imports,
                    1.0,
                    "import".to_string(),
                );
            }
        }
    }

    fn emit_heritage_edges(&mut self) {
        let parsed = self.parsed.to_vec();
        for pf in &parsed {
            for site in &pf.reference_sites {
                let kind = match site.kind {
                    RefKind::Extends => EdgeKind::Extends,
                    RefKind::Implements => EdgeKind::Implements,
                    _ => continue,
                };
                let Some(dst) = self.resolve_type_node(pf, &site.name) else {
                    let ext = self
                        .index
                        .resolve_type(&site.name, &pf.file)
                        .filter(|f| f.contains('.') && !self.index.is_known_type(f));
                    self.push_unresolved(pf, site, "heritage_type_unknown", None, ext);
                    continue;
                };
                self.push_edge(
                    site.in_callable.clone(),
                    dst,
                    kind,
                    1.0,
                    "heritage".to_string(),
                );
            }
        }
    }

    fn emit_mro_edges(&mut self) {
        let mro_map = build_mro_map(&self.index);

        // Pre-collect (owner_fqcn, src_id, method_name, arity) to avoid borrow conflicts
        // with the push_edge mutable borrow that follows.
        let method_entries: Vec<(String, NodeId, String, u16)> = self
            .index
            .methods
            .iter()
            .flat_map(|((owner, name), overloads)| {
                overloads.iter().map(move |def| {
                    (
                        owner.clone(),
                        def.id.clone(),
                        name.clone(),
                        def.param_types.len() as u16,
                    )
                })
            })
            .collect();

        for (owner_fqcn, src_id, name, arity) in method_entries {
            let Some(mro) = mro_map.get(&owner_fqcn) else {
                continue;
            };
            let mut class_override_emitted = false;
            for ancestor in &mro[1..] {
                let dst_id = self.index.find_member(ancestor, &name, Some(arity));
                let is_iface = self.index.is_interface_type(ancestor);
                let Some(dst_id) = dst_id else { continue };
                if is_iface {
                    self.push_edge(
                        src_id.clone(),
                        dst_id,
                        EdgeKind::MethodImplements,
                        1.0,
                        "mro".to_string(),
                    );
                } else if !class_override_emitted {
                    self.push_edge(
                        src_id.clone(),
                        dst_id,
                        EdgeKind::MethodOverrides,
                        1.0,
                        "mro".to_string(),
                    );
                    class_override_emitted = true;
                }
            }
        }
    }

    fn resolve_receiver_bound_call(
        &mut self,
        pf: &ParsedFile,
        site: &ReferenceSite,
    ) -> Option<(NodeId, f32, String)> {
        let receiver = site.receiver.as_deref()?.trim();
        if receiver.is_empty() {
            return None;
        }

        if receiver == "super" {
            let owner = class_of(&site.in_fqcn);
            if let Some(super_fqcn) = self.index.supertypes(owner).first() {
                if let Some(dst) = self
                    .index
                    .find_member_in_hierarchy(super_fqcn, &site.name, site.arity)
                {
                    return Some((dst, 0.8, "receiver-super".to_string()));
                }
            }
            return None;
        }

        if let Some(owner) = self.resolve_receiver_expr_type(pf, site, receiver) {
            // DI redirect: interface receiver with exactly one @Service impl → use the impl.
            let effective_owner = if self.index.is_interface_type(&owner) {
                self.index.di_impl(&owner).unwrap_or_else(|| owner.clone())
            } else {
                owner.clone()
            };

            if let Some(dst) =
                self.index
                    .find_member_in_hierarchy(&effective_owner, &site.name, site.arity)
            {
                let (confidence, reason) = if effective_owner != owner {
                    (0.9, "di-resolved")
                } else if receiver.contains('.') || receiver.contains('(') {
                    (0.7, "receiver-bound")
                } else {
                    (1.0, "receiver-bound")
                };
                return Some((dst, confidence, reason.to_string()));
            }
            if owner.contains('.') && !self.index.is_known_type(&owner) {
                self.unresolved_external_fqcns.insert(owner);
            }
        }

        None
    }

    fn resolve_constructor(&mut self, pf: &ParsedFile, site: &ReferenceSite) -> Option<NodeId> {
        let fqcn = self.index.resolve_type(&site.name, &pf.file)?;
        if let Some(id) = self.index.find_constructor(&fqcn, site.arity) {
            Some(id)
        } else {
            if fqcn.contains('.') && !self.index.is_known_type(&fqcn) {
                self.unresolved_external_fqcns.insert(fqcn);
            }
            None
        }
    }

    fn resolve_field_access(&mut self, pf: &ParsedFile, site: &ReferenceSite) -> Option<NodeId> {
        let owner = match site.receiver.as_deref() {
            Some(receiver) => self.resolve_receiver_expr_type(pf, site, receiver)?,
            None => class_of(&site.in_fqcn).to_string(),
        };
        self.index.find_field_in_hierarchy(&owner, &site.name)
    }

    fn resolve_type_node(&mut self, pf: &ParsedFile, raw: &str) -> Option<NodeId> {
        let fqcn = self.index.resolve_type(raw, &pf.file)?;
        let id = self.index.type_node_id(&fqcn);
        if id.is_none() && fqcn.contains('.') {
            self.unresolved_external_fqcns.insert(fqcn);
        }
        id
    }

    fn resolve_import_target(&self, pf: &ParsedFile, import: &RawImport) -> Option<NodeId> {
        if import.is_wildcard {
            return None;
        }

        let raw_type = if import.is_static {
            import.raw.rsplit_once('.').map(|(owner, _)| owner)?
        } else {
            import.raw.as_str()
        };
        let fqcn = self.index.resolve_type(raw_type, &pf.file)?;
        self.index.type_node_id(&fqcn)
    }

    fn find_static_imported_member(
        &self,
        pf: &ParsedFile,
        name: &str,
        arity: Option<u16>,
    ) -> Option<NodeId> {
        for import in &pf.imports {
            if !import.is_static {
                continue;
            }
            if import.is_wildcard {
                let owner = import.raw.trim_end_matches(".*");
                if let Some(dst) = self.index.find_member(owner, name, arity) {
                    return Some(dst);
                }
            } else if let Some((owner, imported_name)) = import.raw.rsplit_once('.') {
                if imported_name == name {
                    if let Some(dst) = self.index.find_member(owner, name, arity) {
                        return Some(dst);
                    }
                }
            }
        }
        None
    }

    fn resolve_receiver_expr_type(
        &mut self,
        pf: &ParsedFile,
        site: &ReferenceSite,
        receiver: &str,
    ) -> Option<String> {
        let receiver = receiver.trim();
        if receiver.is_empty() {
            return None;
        }

        if is_simple_ident(receiver) {
            if receiver == "this" || receiver == "super" {
                return self.index.receiver_type(&site.in_fqcn, receiver);
            }
            if starts_uppercase(receiver) {
                if let Some(fqcn) = self.index.resolve_type(receiver, &pf.file) {
                    if self.index.is_known_type(&fqcn) {
                        return Some(fqcn);
                    }
                }
            }
            return self.index.receiver_type(&site.in_fqcn, receiver);
        }

        if !receiver.contains('.') && receiver.ends_with(')') {
            let call = call_name(receiver)?;
            let owner = class_of(&site.in_fqcn);
            return self
                .index
                .member_return_type_in_hierarchy(owner, call, None);
        }

        if let Some(fqcn) = self.index.resolve_type(receiver, &pf.file) {
            if self.index.is_known_type(&fqcn) {
                return Some(fqcn);
            }
        }

        if let Some((left, right)) = split_last_dot_outside_parens(receiver) {
            if starts_uppercase(left) {
                if let Some(fqcn) = self.index.resolve_type(left, &pf.file) {
                    if self.index.is_known_type(&fqcn) {
                        if right.ends_with(')') {
                            let name = call_name(right)?;
                            return self
                                .index
                                .member_return_type_in_hierarchy(&fqcn, name, None);
                        }
                        return self.index.field_type_in_hierarchy(&fqcn, right);
                    }
                }
            }

            let owner = self.resolve_receiver_expr_type(pf, site, left)?;
            if right.ends_with(')') {
                let name = call_name(right)?;
                return self
                    .index
                    .member_return_type_in_hierarchy(&owner, name, None);
            }
            return self.index.field_type_in_hierarchy(&owner, right);
        }

        None
    }

    fn push_edge(
        &mut self,
        src: NodeId,
        dst: NodeId,
        kind: EdgeKind,
        confidence: f32,
        reason: String,
    ) {
        if src.as_str() == "Method:<unknown>" || dst.as_str().is_empty() {
            self.skipped += 1;
            return;
        }
        self.edges.push(Edge {
            src,
            dst,
            kind,
            confidence,
            reason,
        });
    }

    fn finish(mut self) -> ResolveOutput {
        let mut deduped_nodes = BTreeMap::new();
        for node in self.nodes.drain(..) {
            deduped_nodes
                .entry(node.id.as_str().to_string())
                .or_insert(node);
        }
        let mut deduped = BTreeMap::new();
        for edge in self.edges.drain(..) {
            let key = (
                edge.src.as_str().to_string(),
                edge.dst.as_str().to_string(),
                edge.kind.cypher_label(),
            );
            deduped.entry(key).or_insert(edge);
        }
        let edges = deduped.into_values().collect();
        ResolveOutput {
            nodes: deduped_nodes.into_values().collect(),
            edges,
            skipped: self.skipped,
            unresolved_external_fqcns: self.unresolved_external_fqcns.into_iter().collect(),
            unresolved_refs: self.unresolved_refs,
        }
    }
}

/// Convert parser-discovered inter-service contract sites into graph nodes and edges.
pub fn resolve_contract_edges(parsed: &[ParsedFile]) -> (Vec<Node>, Vec<Edge>) {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    for pf in parsed {
        for site in &pf.contract_sites {
            match site.kind {
                ContractKind::HttpCall | ContractKind::FeignClient => {
                    let Some(url_template) = site.url_template.as_deref() else {
                        continue;
                    };
                    let Some(http_method) = site.http_method.as_deref() else {
                        continue;
                    };
                    let method = http_method.to_ascii_uppercase();
                    let id = external_endpoint_id(&method, url_template);
                    let name = format!("{method} {url_template}");
                    let source = match site.kind {
                        ContractKind::FeignClient => "feign-client",
                        _ => "http-client",
                    };
                    nodes.push(Node {
                        id: id.clone(),
                        kind: NodeKind::ExternalEndpoint,
                        name: name.clone(),
                        qualified_name: Some(name),
                        file: pf.file.clone(),
                        range: site.range,
                        props: Some(serde_json::json!({
                            "httpMethod": method,
                            "path": url_template,
                            "urlTemplate": url_template,
                            "source": source,
                        })),
                    });
                    edges.push(Edge {
                        src: site.in_callable.clone(),
                        dst: id,
                        kind: EdgeKind::ExternalCall,
                        confidence: 0.75,
                        reason: match site.kind {
                            ContractKind::FeignClient => "feign-client",
                            _ => "http-client",
                        }
                        .to_string(),
                    });
                }
                ContractKind::EventPublish | ContractKind::EventListen => {
                    let Some(topic) = site.topic.as_deref() else {
                        continue;
                    };
                    let id = kafka_topic_id(topic);
                    nodes.push(Node {
                        id: id.clone(),
                        kind: NodeKind::KafkaTopic,
                        name: topic.to_string(),
                        qualified_name: Some(topic.to_string()),
                        file: pf.file.clone(),
                        range: site.range,
                        props: Some(serde_json::json!({
                            "topic": topic,
                        })),
                    });
                    let (kind, reason) = match site.kind {
                        ContractKind::EventPublish => (EdgeKind::PublishesEvent, "event-publish"),
                        ContractKind::EventListen => (EdgeKind::ListensTo, "event-listen"),
                        _ => unreachable!("HTTP contract kind handled above"),
                    };
                    edges.push(Edge {
                        src: site.in_callable.clone(),
                        dst: id,
                        kind,
                        confidence: 0.8,
                        reason: reason.to_string(),
                    });
                }
            }
        }
    }

    let mut deduped_nodes = BTreeMap::new();
    for node in nodes {
        deduped_nodes
            .entry(node.id.as_str().to_string())
            .or_insert(node);
    }
    let mut deduped_edges = BTreeMap::new();
    for edge in edges {
        let key = (
            edge.src.as_str().to_string(),
            edge.dst.as_str().to_string(),
            edge.kind.cypher_label(),
        );
        deduped_edges.entry(key).or_insert(edge);
    }

    (
        deduped_nodes.into_values().collect(),
        deduped_edges.into_values().collect(),
    )
}

/// Remove duplicates from `v` without changing the order of the first occurrences.
fn stable_dedup(v: &mut Vec<String>) {
    let mut seen = HashSet::new();
    v.retain(|x| seen.insert(x.clone()));
}

/// Compute a C3 linearization for every type in the index.
/// Result: type FQCN → ordered MRO list (self first, then ancestors breadth-first in C3 order).
fn build_mro_map(index: &ResolveIndex) -> HashMap<String, Vec<String>> {
    let mut cache: HashMap<String, Vec<String>> = HashMap::new();
    let all: Vec<String> = index.type_fqcns().map(str::to_string).collect();
    for fqcn in &all {
        c3_linearize(fqcn, index, &mut cache);
    }
    cache
}

/// C3 linearization of `fqcn`. Results are memoized in `cache`.
/// Supertypes must be ordered: superclass first (if any), then interfaces — this is guaranteed
/// by [`ResolveIndex::dedup`] which uses [`stable_dedup`] and the parse order from java.rs.
fn c3_linearize(
    fqcn: &str,
    index: &ResolveIndex,
    cache: &mut HashMap<String, Vec<String>>,
) -> Vec<String> {
    if let Some(cached) = cache.get(fqcn) {
        return cached.clone();
    }
    // Pre-insert sentinel so cycles in the supertype graph don't loop forever.
    cache.insert(fqcn.to_string(), vec![fqcn.to_string()]);

    let bases: Vec<String> = index.supertypes(fqcn).to_vec();
    if bases.is_empty() {
        return vec![fqcn.to_string()];
    }

    // Build the merge input: one linearization per base, plus the bases list itself.
    let mut lists: Vec<Vec<String>> = bases
        .iter()
        .map(|b| c3_linearize(b, index, cache))
        .collect();
    lists.push(bases);

    let mut result = vec![fqcn.to_string()];
    loop {
        lists.retain(|l| !l.is_empty());
        if lists.is_empty() {
            break;
        }
        // Pick the first head that is not in the tail of any list.
        let head = lists
            .iter()
            .find_map(|list| {
                let h = &list[0];
                let blocked = lists.iter().any(|l| l.len() > 1 && l[1..].contains(h));
                if !blocked {
                    Some(h.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| lists[0][0].clone()); // cycle fallback: take first
        result.push(head.clone());
        for list in &mut lists {
            list.retain(|x| x != &head);
        }
    }

    cache.insert(fqcn.to_string(), result.clone());
    result
}

fn is_type_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class
            | NodeKind::Interface
            | NodeKind::Enum
            | NodeKind::Record
            | NodeKind::Annotation
    )
}

/// Simple (last) segment of a dotted FQCN.
fn simple_of(fqcn: &str) -> String {
    fqcn.rsplit('.').next().unwrap_or(fqcn).to_string()
}

/// Enclosing class FQCN of a callable signature (`fqcn#name/arity` → `fqcn`).
fn class_of(in_fqcn: &str) -> &str {
    in_fqcn.split('#').next().unwrap_or(in_fqcn)
}

/// Strip generics and array brackets to the base type name.
fn base_type_name(raw: &str) -> String {
    raw.split('<')
        .next()
        .unwrap_or(raw)
        .replace("[]", "")
        .trim()
        .to_string()
}

/// Choose the best binding for `name`: by kind precedence, then latest range
/// (nearest declaration wins for shadowing).
fn pick_binding<'a>(bindings: &'a [TypeBinding], name: &str) -> Option<&'a TypeBinding> {
    bindings.iter().filter(|b| b.name == name).max_by(|a, b| {
        binding_rank(a.kind)
            .cmp(&binding_rank(b.kind))
            .then(a.range.start_line.cmp(&b.range.start_line))
            .then(a.range.start_col.cmp(&b.range.start_col))
    })
}

/// Higher rank wins. Params/locals beat patterns; aliases/call-results last.
fn binding_rank(kind: BindingKind) -> u8 {
    match kind {
        BindingKind::Param => 6,
        BindingKind::Local => 5,
        BindingKind::Pattern => 4,
        BindingKind::Field => 3,
        BindingKind::CallResult => 2,
        BindingKind::Alias => 1,
        BindingKind::Return => 0,
    }
}

fn is_simple_ident(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn starts_uppercase(value: &str) -> bool {
    value
        .chars()
        .next()
        .map(|ch| ch.is_ascii_uppercase())
        .unwrap_or(false)
}

fn call_name(expr: &str) -> Option<&str> {
    let open = expr.rfind('(')?;
    let name = expr[..open].trim();
    (!name.is_empty()).then_some(name)
}

fn split_last_dot_outside_parens(value: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    for (idx, ch) in value.char_indices().rev() {
        match ch {
            ')' => depth += 1,
            '(' => depth = depth.saturating_sub(1),
            '.' if depth == 0 => {
                let left = value[..idx].trim();
                let right = value[idx + 1..].trim();
                if !left.is_empty() && !right.is_empty() {
                    return Some((left, right));
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{
        constructor_id, external_endpoint_id, field_id, kafka_topic_id, method_id, type_id,
        ContractKind, ContractSite, EdgeKind, Range, ReferenceSite,
    };

    fn type_def(kind: NodeKind, fqcn: &str) -> SymbolDef {
        SymbolDef {
            id: type_id(kind, fqcn),
            kind,
            fqcn: fqcn.into(),
            name: simple_of(fqcn),
            owner: None,
            range: Range::default(),
            modifiers: Vec::new(),
            param_types: Vec::new(),
            return_type: None,
            declared_type: None,
            stereotype: None,
        }
    }

    fn method_def(owner: &str, name: &str, params: &[&str], ret: Option<&str>) -> SymbolDef {
        SymbolDef {
            id: method_id(owner, name, params.len() as u16),
            kind: NodeKind::Method,
            fqcn: owner.into(),
            name: name.into(),
            owner: Some(type_id(NodeKind::Class, owner)),
            range: Range::default(),
            modifiers: Vec::new(),
            param_types: params.iter().map(|s| s.to_string()).collect(),
            return_type: ret.map(str::to_string),
            declared_type: None,
            stereotype: None,
        }
    }

    fn field_def(owner: &str, name: &str, ty: &str) -> SymbolDef {
        SymbolDef {
            id: field_id(owner, name),
            kind: NodeKind::Field,
            fqcn: owner.into(),
            name: name.into(),
            owner: Some(type_id(NodeKind::Class, owner)),
            range: Range::default(),
            modifiers: Vec::new(),
            param_types: Vec::new(),
            return_type: None,
            declared_type: Some(ty.into()),
            stereotype: None,
        }
    }

    fn ctor_def(owner: &str, params: &[&str]) -> SymbolDef {
        SymbolDef {
            id: constructor_id(owner, params.len() as u16),
            kind: NodeKind::Constructor,
            fqcn: owner.into(),
            name: "<init>".into(),
            owner: Some(type_id(NodeKind::Class, owner)),
            range: Range::default(),
            modifiers: Vec::new(),
            param_types: params.iter().map(|s| s.to_string()).collect(),
            return_type: None,
            declared_type: None,
            stereotype: None,
        }
    }

    fn binding(name: &str, raw: &str, kind: BindingKind, in_fqcn: &str, line: u32) -> TypeBinding {
        TypeBinding {
            name: name.into(),
            raw_type: raw.into(),
            kind,
            in_fqcn: in_fqcn.into(),
            range: Range {
                start_line: line,
                ..Range::default()
            },
        }
    }

    fn import(raw: &str) -> RawImport {
        RawImport {
            raw: raw.into(),
            is_static: false,
            is_wildcard: raw.ends_with(".*"),
            range: Range::default(),
        }
    }

    #[test]
    fn contract_sites_emit_nodes_and_edges() {
        let caller = method_id("com.acme.Client", "call", 0);
        let listener = method_id("com.acme.Client", "listen", 1);
        let file = ParsedFile {
            file: "com/acme/Client.java".into(),
            package: Some("com.acme".into()),
            defs: vec![],
            imports: vec![],
            reference_sites: vec![],
            type_bindings: vec![],
            contract_sites: vec![
                ContractSite {
                    kind: ContractKind::HttpCall,
                    url_template: Some("/api/orders/{id}".into()),
                    topic: None,
                    http_method: Some("get".into()),
                    in_callable: caller.clone(),
                    range: Range::default(),
                },
                ContractSite {
                    kind: ContractKind::EventPublish,
                    url_template: None,
                    topic: Some("orders.created".into()),
                    http_method: None,
                    in_callable: caller.clone(),
                    range: Range::default(),
                },
                ContractSite {
                    kind: ContractKind::EventListen,
                    url_template: None,
                    topic: Some("orders.created".into()),
                    http_method: None,
                    in_callable: listener.clone(),
                    range: Range::default(),
                },
            ],
        };

        let out = resolve_edges(&[file]);
        let endpoint = external_endpoint_id("GET", "/api/orders/{id}");
        let topic = kafka_topic_id("orders.created");
        assert!(out
            .nodes
            .iter()
            .any(|node| node.id == endpoint && node.kind == NodeKind::ExternalEndpoint));
        assert!(out
            .nodes
            .iter()
            .any(|node| node.id == topic && node.kind == NodeKind::KafkaTopic));
        assert!(out.edges.iter().any(|edge| {
            edge.kind == EdgeKind::ExternalCall && edge.src == caller && edge.dst == endpoint
        }));
        assert!(out.edges.iter().any(|edge| {
            edge.kind == EdgeKind::PublishesEvent && edge.src == caller && edge.dst == topic
        }));
        assert!(out.edges.iter().any(|edge| {
            edge.kind == EdgeKind::ListensTo && edge.src == listener && edge.dst == topic
        }));
    }

    fn heritage(class_fqcn: &str, super_name: &str, kind: RefKind) -> ReferenceSite {
        ReferenceSite {
            name: super_name.into(),
            receiver: None,
            kind,
            arity: None,
            range: Range::default(),
            in_fqcn: class_fqcn.into(),
            in_callable: type_id(NodeKind::Class, class_fqcn),
        }
    }

    fn ref_site(
        in_fqcn: &str,
        in_callable: NodeId,
        kind: RefKind,
        receiver: Option<&str>,
        name: &str,
        arity: Option<u16>,
    ) -> ReferenceSite {
        ReferenceSite {
            name: name.into(),
            receiver: receiver.map(str::to_string),
            kind,
            arity,
            range: Range::default(),
            in_fqcn: in_fqcn.into(),
            in_callable,
        }
    }

    /// Workspace: an interface `Repo` with `findAll()`, a class `OwnerService`
    /// implementing it, and an `OwnerController` with a `service` field + `handle`
    /// method taking an `OwnerService svc` param.
    fn workspace() -> Vec<ParsedFile> {
        let repo = ParsedFile {
            file: "com/acme/Repo.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                type_def(NodeKind::Interface, "com.acme.Repo"),
                method_def("com.acme.Repo", "findAll", &[], Some("List")),
            ],
            imports: vec![import("java.util.List")],
            reference_sites: vec![],
            type_bindings: vec![],
            contract_sites: vec![],
        };
        let service = ParsedFile {
            file: "com/acme/OwnerService.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                type_def(NodeKind::Class, "com.acme.OwnerService"),
                method_def("com.acme.OwnerService", "save", &["Owner"], None),
                method_def("com.acme.OwnerService", "save", &["Owner", "boolean"], None),
            ],
            imports: vec![],
            reference_sites: vec![heritage(
                "com.acme.OwnerService",
                "Repo",
                RefKind::Implements,
            )],
            type_bindings: vec![],
            contract_sites: vec![],
        };
        let controller = ParsedFile {
            file: "com/acme/OwnerController.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                type_def(NodeKind::Class, "com.acme.OwnerController"),
                type_def(NodeKind::Class, "com.acme.Owner"),
                ctor_def("com.acme.Owner", &[]),
                field_def("com.acme.OwnerController", "service", "OwnerService"),
                method_def(
                    "com.acme.OwnerController",
                    "handle",
                    &["OwnerService"],
                    None,
                ),
            ],
            imports: vec![import("java.util.List"), import("com.other.Thing")],
            reference_sites: vec![],
            type_bindings: vec![binding(
                "svc",
                "OwnerService",
                BindingKind::Param,
                "com.acme.OwnerController#handle/1",
                5,
            )],
            contract_sites: vec![],
        };
        let thing = ParsedFile {
            file: "com/other/Thing.java".into(),
            package: Some("com.other".into()),
            defs: vec![type_def(NodeKind::Class, "com.other.Thing")],
            imports: vec![],
            reference_sites: vec![],
            type_bindings: vec![],
            contract_sites: vec![],
        };
        vec![repo, service, controller, thing]
    }

    #[test]
    fn resolve_type_uses_import_same_package_and_generics() {
        let idx = ResolveIndex::build(&workspace());
        let f = "com/acme/OwnerController.java";
        assert_eq!(
            idx.resolve_type("List", f).as_deref(),
            Some("java.util.List")
        );
        assert_eq!(
            idx.resolve_type("Thing", f).as_deref(),
            Some("com.other.Thing")
        );
        assert_eq!(
            idx.resolve_type("Owner", f).as_deref(),
            Some("com.acme.Owner")
        ); // same package, known
        assert_eq!(
            idx.resolve_type("List<Owner>", f).as_deref(),
            Some("java.util.List")
        ); // generics stripped
        assert_eq!(idx.resolve_type("Nope", f), None);
    }

    #[test]
    fn find_member_matches_overload_by_arity() {
        let idx = ResolveIndex::build(&workspace());
        assert_eq!(
            idx.find_member("com.acme.OwnerService", "save", Some(1)),
            Some(method_id("com.acme.OwnerService", "save", 1))
        );
        assert_eq!(
            idx.find_member("com.acme.OwnerService", "save", Some(2)),
            Some(method_id("com.acme.OwnerService", "save", 2))
        );
        assert_eq!(
            idx.find_member("com.acme.OwnerService", "missing", Some(0)),
            None
        );
    }

    #[test]
    fn receiver_type_param_field_and_this() {
        let idx = ResolveIndex::build(&workspace());
        let scope = "com.acme.OwnerController#handle/1";
        // param `svc`
        assert_eq!(
            idx.receiver_type(scope, "svc").as_deref(),
            Some("com.acme.OwnerService")
        );
        // field `service` (no local shadows it)
        assert_eq!(
            idx.receiver_type(scope, "service").as_deref(),
            Some("com.acme.OwnerService")
        );
        // `this`
        assert_eq!(
            idx.receiver_type(scope, "this").as_deref(),
            Some("com.acme.OwnerController")
        );
        assert_eq!(idx.receiver_type(scope, "unknown"), None);
    }

    #[test]
    fn local_param_shadows_field() {
        let mut files = workspace();
        // Add a local `service` of a different type inside handle/1 — must win over the field.
        files[2].type_bindings.push(binding(
            "service",
            "Owner",
            BindingKind::Local,
            "com.acme.OwnerController#handle/1",
            6,
        ));
        let idx = ResolveIndex::build(&files);
        assert_eq!(
            idx.receiver_type("com.acme.OwnerController#handle/1", "service")
                .as_deref(),
            Some("com.acme.Owner"),
            "a local must shadow the field of the same name"
        );
    }

    #[test]
    fn heritage_and_inherited_member_lookup() {
        let idx = ResolveIndex::build(&workspace());
        assert_eq!(idx.supertypes("com.acme.OwnerService"), ["com.acme.Repo"]);
        assert_eq!(idx.implementors("com.acme.Repo"), ["com.acme.OwnerService"]);
        // findAll is declared on the interface; resolves through the hierarchy.
        assert_eq!(
            idx.find_member("com.acme.OwnerService", "findAll", Some(0)),
            None
        );
        assert_eq!(
            idx.find_member_in_hierarchy("com.acme.OwnerService", "findAll", Some(0)),
            Some(method_id("com.acme.Repo", "findAll", 0))
        );
    }

    #[test]
    fn phase_4_2_receiver_bound_call_emits_calls_edge() {
        let mut files = workspace();
        let scope = "com.acme.OwnerController#handle/1";
        files[2].reference_sites.push(ref_site(
            scope,
            method_id("com.acme.OwnerController", "handle", 1),
            RefKind::Call,
            Some("service"),
            "save",
            Some(1),
        ));

        let out = resolve_edges(&files);
        assert!(
            out.edges.iter().any(|edge| {
                edge.kind == EdgeKind::Calls
                    && edge.src == method_id("com.acme.OwnerController", "handle", 1)
                    && edge.dst == method_id("com.acme.OwnerService", "save", 1)
            }),
            "service.save(owner) should resolve to OwnerService#save/1"
        );
        assert_eq!(out.skipped, 0);
    }

    #[test]
    fn phase_4_2_free_calls_imports_heritage_fields_and_ctors() {
        let mut files = workspace();
        files[1].reference_sites.push(ref_site(
            "com.acme.OwnerService#save/1",
            method_id("com.acme.OwnerService", "save", 1),
            RefKind::Call,
            None,
            "findAll",
            Some(0),
        ));
        files[2].reference_sites.push(ref_site(
            "com.acme.OwnerController#handle/1",
            method_id("com.acme.OwnerController", "handle", 1),
            RefKind::FieldRead,
            Some("this"),
            "service",
            None,
        ));
        files[2].reference_sites.push(ref_site(
            "com.acme.OwnerController#handle/1",
            method_id("com.acme.OwnerController", "handle", 1),
            RefKind::Ctor,
            None,
            "Owner",
            Some(0),
        ));

        let out = resolve_edges(&files);
        assert!(out.edges.iter().any(|edge| {
            edge.kind == EdgeKind::Calls
                && edge.src == method_id("com.acme.OwnerService", "save", 1)
                && edge.dst == method_id("com.acme.Repo", "findAll", 0)
        }));
        assert!(out.edges.iter().any(|edge| {
            edge.kind == EdgeKind::Accesses
                && edge.src == method_id("com.acme.OwnerController", "handle", 1)
                && edge.dst == field_id("com.acme.OwnerController", "service")
        }));
        assert!(out.edges.iter().any(|edge| {
            edge.kind == EdgeKind::Calls
                && edge.src == method_id("com.acme.OwnerController", "handle", 1)
                && edge.dst == constructor_id("com.acme.Owner", 0)
        }));
        assert!(out.edges.iter().any(|edge| {
            edge.kind == EdgeKind::Imports
                && edge.src == file_id("com/acme/OwnerController.java")
                && edge.dst == type_id(NodeKind::Class, "com.other.Thing")
        }));
        assert!(out.edges.iter().any(|edge| {
            edge.kind == EdgeKind::Implements
                && edge.src == type_id(NodeKind::Class, "com.acme.OwnerService")
                && edge.dst == type_id(NodeKind::Interface, "com.acme.Repo")
        }));
        assert_eq!(out.skipped, 0);
    }

    #[test]
    fn phase_4_2_unresolved_external_receiver_is_reported() {
        let mut files = workspace();
        let scope = "com.acme.OwnerController#handle/1";
        files[2].type_bindings.push(binding(
            "client",
            "com.external.Client",
            BindingKind::Local,
            scope,
            7,
        ));
        files[2].reference_sites.push(ref_site(
            scope,
            method_id("com.acme.OwnerController", "handle", 1),
            RefKind::Call,
            Some("client"),
            "fetch",
            Some(0),
        ));

        let out = resolve_edges(&files);
        assert_eq!(out.skipped, 1);
        assert_eq!(out.unresolved_external_fqcns, vec!["com.external.Client"]);
    }

    // ── Phase 4.3 MRO tests ────────────────────────────────────────────────

    /// Minimal hierarchy shared by the MRO tests:
    ///   interface Animal { void speak(); }
    ///   abstract class Mammal implements Animal { void breathe(); }
    ///   class Dog extends Mammal implements Animal { void speak(); void breathe(); }
    fn mro_workspace() -> Vec<ParsedFile> {
        let animal = ParsedFile {
            file: "com/acme/Animal.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                type_def(NodeKind::Interface, "com.acme.Animal"),
                method_def("com.acme.Animal", "speak", &[], None),
            ],
            imports: vec![],
            reference_sites: vec![],
            type_bindings: vec![],
            contract_sites: vec![],
        };
        let mammal = ParsedFile {
            file: "com/acme/Mammal.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                type_def(NodeKind::Class, "com.acme.Mammal"),
                method_def("com.acme.Mammal", "breathe", &[], None),
            ],
            imports: vec![],
            // Mammal implements Animal but has no speak() — abstract.
            reference_sites: vec![heritage("com.acme.Mammal", "Animal", RefKind::Implements)],
            type_bindings: vec![],
            contract_sites: vec![],
        };
        let dog = ParsedFile {
            file: "com/acme/Dog.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                type_def(NodeKind::Class, "com.acme.Dog"),
                method_def("com.acme.Dog", "speak", &[], None),
                method_def("com.acme.Dog", "breathe", &[], None),
            ],
            imports: vec![],
            // extends first, then implements — preserves C3 order.
            reference_sites: vec![
                heritage("com.acme.Dog", "Mammal", RefKind::Extends),
                heritage("com.acme.Dog", "Animal", RefKind::Implements),
            ],
            type_bindings: vec![],
            contract_sites: vec![],
        };
        vec![animal, mammal, dog]
    }

    #[test]
    fn phase_4_3_mro_method_implements_interface() {
        let files = mro_workspace();
        let out = resolve_edges(&files);
        assert!(
            out.edges.iter().any(|e| {
                e.kind == EdgeKind::MethodImplements
                    && e.src == method_id("com.acme.Dog", "speak", 0)
                    && e.dst == method_id("com.acme.Animal", "speak", 0)
            }),
            "Dog.speak should METHOD_IMPLEMENTS Animal.speak"
        );
    }

    #[test]
    fn phase_4_3_mro_method_overrides_superclass() {
        let files = mro_workspace();
        let out = resolve_edges(&files);
        assert!(
            out.edges.iter().any(|e| {
                e.kind == EdgeKind::MethodOverrides
                    && e.src == method_id("com.acme.Dog", "breathe", 0)
                    && e.dst == method_id("com.acme.Mammal", "breathe", 0)
            }),
            "Dog.breathe should METHOD_OVERRIDES Mammal.breathe"
        );
    }

    #[test]
    fn phase_4_3_mro_both_overrides_and_implements() {
        // Add speak() to Mammal so Dog.speak overrides it AND implements Animal.speak.
        let mut files = mro_workspace();
        files[1]
            .defs
            .push(method_def("com.acme.Mammal", "speak", &[], None));
        let out = resolve_edges(&files);
        // Dog.speak METHOD_OVERRIDES Mammal.speak (nearest class ancestor).
        assert!(
            out.edges.iter().any(|e| {
                e.kind == EdgeKind::MethodOverrides
                    && e.src == method_id("com.acme.Dog", "speak", 0)
                    && e.dst == method_id("com.acme.Mammal", "speak", 0)
            }),
            "Dog.speak should METHOD_OVERRIDES Mammal.speak"
        );
        // Dog.speak METHOD_IMPLEMENTS Animal.speak (interface in MRO).
        assert!(
            out.edges.iter().any(|e| {
                e.kind == EdgeKind::MethodImplements
                    && e.src == method_id("com.acme.Dog", "speak", 0)
                    && e.dst == method_id("com.acme.Animal", "speak", 0)
            }),
            "Dog.speak should also METHOD_IMPLEMENTS Animal.speak"
        );
    }

    #[test]
    fn phase_4_3_c3_order_superclass_before_interface() {
        // Verifies that the MRO puts the direct superclass before the interface when
        // both have the same method, so METHOD_OVERRIDES fires before METHOD_IMPLEMENTS.
        let base = ParsedFile {
            file: "com/acme/Base.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                type_def(NodeKind::Class, "com.acme.Base"),
                method_def("com.acme.Base", "act", &[], None),
            ],
            imports: vec![],
            reference_sites: vec![],
            type_bindings: vec![],
            contract_sites: vec![],
        };
        let marker = ParsedFile {
            file: "com/acme/Marker.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                type_def(NodeKind::Interface, "com.acme.Marker"),
                method_def("com.acme.Marker", "act", &[], None),
            ],
            imports: vec![],
            reference_sites: vec![],
            type_bindings: vec![],
            contract_sites: vec![],
        };
        let child = ParsedFile {
            file: "com/acme/Child.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                type_def(NodeKind::Class, "com.acme.Child"),
                method_def("com.acme.Child", "act", &[], None),
            ],
            imports: vec![],
            // extends Base, implements Marker — C3: [Child, Base, Marker]
            reference_sites: vec![
                heritage("com.acme.Child", "Base", RefKind::Extends),
                heritage("com.acme.Child", "Marker", RefKind::Implements),
            ],
            type_bindings: vec![],
            contract_sites: vec![],
        };
        let out = resolve_edges(&[base, marker, child]);
        // Exactly one METHOD_OVERRIDES to Base.act (not to Marker).
        assert!(
            out.edges.iter().any(|e| {
                e.kind == EdgeKind::MethodOverrides
                    && e.src == method_id("com.acme.Child", "act", 0)
                    && e.dst == method_id("com.acme.Base", "act", 0)
            }),
            "Child.act should METHOD_OVERRIDES Base.act"
        );
        assert!(
            out.edges.iter().any(|e| {
                e.kind == EdgeKind::MethodImplements
                    && e.src == method_id("com.acme.Child", "act", 0)
                    && e.dst == method_id("com.acme.Marker", "act", 0)
            }),
            "Child.act should METHOD_IMPLEMENTS Marker.act"
        );
        // No METHOD_OVERRIDES to Marker (it's an interface).
        assert!(
            !out.edges.iter().any(|e| {
                e.kind == EdgeKind::MethodOverrides
                    && e.dst == method_id("com.acme.Marker", "act", 0)
            }),
            "should not emit METHOD_OVERRIDES to an interface"
        );
    }

    // ── Spring DI resolution tests ────────────────────────────────────────────

    fn make_di_scenario(impl_stereotype: Option<&str>) -> Vec<ParsedFile> {
        // Interface: UserService with save(User)
        let iface = ParsedFile {
            file: "com/acme/UserService.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                SymbolDef {
                    id: type_id(NodeKind::Interface, "com.acme.UserService"),
                    kind: NodeKind::Interface,
                    fqcn: "com.acme.UserService".into(),
                    name: "UserService".into(),
                    owner: None,
                    range: Range::default(),
                    modifiers: Vec::new(),
                    param_types: Vec::new(),
                    return_type: None,
                    declared_type: None,
                    stereotype: None,
                },
                method_def("com.acme.UserService", "save", &["User"], None),
            ],
            imports: vec![],
            reference_sites: vec![],
            type_bindings: vec![],
            contract_sites: vec![],
        };

        // Impl: UserServiceImpl implements UserService
        let impl_def = SymbolDef {
            id: type_id(NodeKind::Class, "com.acme.UserServiceImpl"),
            kind: NodeKind::Class,
            fqcn: "com.acme.UserServiceImpl".into(),
            name: "UserServiceImpl".into(),
            owner: None,
            range: Range::default(),
            modifiers: Vec::new(),
            param_types: Vec::new(),
            return_type: None,
            declared_type: None,
            stereotype: impl_stereotype.map(str::to_string),
        };
        let impl_file = ParsedFile {
            file: "com/acme/UserServiceImpl.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                impl_def.clone(),
                method_def("com.acme.UserServiceImpl", "save", &["User"], None),
            ],
            imports: vec![],
            reference_sites: vec![heritage(
                "com.acme.UserServiceImpl",
                "UserService",
                RefKind::Implements,
            )],
            type_bindings: vec![],
            contract_sites: vec![],
        };

        // Caller: OrderController with field `userService: UserService` and call userService.save(u)
        let caller = ParsedFile {
            file: "com/acme/OrderController.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                SymbolDef {
                    id: type_id(NodeKind::Class, "com.acme.OrderController"),
                    kind: NodeKind::Class,
                    fqcn: "com.acme.OrderController".into(),
                    name: "OrderController".into(),
                    owner: None,
                    range: Range::default(),
                    modifiers: Vec::new(),
                    param_types: Vec::new(),
                    return_type: None,
                    declared_type: None,
                    stereotype: Some("controller".into()),
                },
                method_def("com.acme.OrderController", "placeOrder", &["Order"], None),
                field_def("com.acme.OrderController", "userService", "UserService"),
            ],
            imports: vec![],
            reference_sites: vec![ReferenceSite {
                name: "save".into(),
                receiver: Some("userService".into()),
                kind: RefKind::Call,
                arity: Some(1),
                range: Range::default(),
                in_fqcn: "com.acme.OrderController#placeOrder/1".into(),
                in_callable: method_id("com.acme.OrderController", "placeOrder", 1),
            }],
            type_bindings: vec![TypeBinding {
                name: "userService".into(),
                raw_type: "UserService".into(),
                kind: BindingKind::Field,
                in_fqcn: "com.acme.OrderController".into(),
                range: Range::default(),
            }],
            contract_sites: vec![],
        };

        vec![iface, impl_file, caller]
    }

    #[test]
    fn di_resolves_interface_call_to_service_impl() {
        let files = make_di_scenario(Some("service"));
        let out = resolve_edges(&files);
        let calls: Vec<_> = out
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect();
        // Should call the impl, not the interface
        assert!(
            calls
                .iter()
                .any(|e| e.dst == method_id("com.acme.UserServiceImpl", "save", 1)),
            "should resolve to impl method"
        );
        assert!(
            !calls
                .iter()
                .any(|e| e.dst == method_id("com.acme.UserService", "save", 1)),
            "should NOT call the interface method when impl is found"
        );
        // Confidence should be 0.9 for DI-resolved
        let di_edge = calls
            .iter()
            .find(|e| e.dst == method_id("com.acme.UserServiceImpl", "save", 1))
            .unwrap();
        assert_eq!(di_edge.reason, "di-resolved");
    }

    #[test]
    fn di_falls_back_when_no_service_impl() {
        // Impl exists but has no @Service stereotype
        let files = make_di_scenario(None);
        let out = resolve_edges(&files);
        let calls: Vec<_> = out
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect();
        // Falls back to interface method
        assert!(
            calls
                .iter()
                .any(|e| e.dst == method_id("com.acme.UserService", "save", 1)),
            "should fall back to interface method when no @Service impl"
        );
    }

    #[test]
    fn di_falls_back_when_multiple_service_impls() {
        // Two @Service impls — ambiguous, must not guess
        let iface = ParsedFile {
            file: "com/acme/UserService.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                SymbolDef {
                    id: type_id(NodeKind::Interface, "com.acme.UserService"),
                    kind: NodeKind::Interface,
                    fqcn: "com.acme.UserService".into(),
                    name: "UserService".into(),
                    owner: None,
                    range: Range::default(),
                    modifiers: Vec::new(),
                    param_types: Vec::new(),
                    return_type: None,
                    declared_type: None,
                    stereotype: None,
                },
                method_def("com.acme.UserService", "save", &["User"], None),
            ],
            imports: vec![],
            reference_sites: vec![],
            type_bindings: vec![],
            contract_sites: vec![],
        };
        let make_impl = |name: &str| -> ParsedFile {
            let fqcn = format!("com.acme.{name}");
            ParsedFile {
                file: format!("com/acme/{name}.java"),
                package: Some("com.acme".into()),
                defs: vec![
                    SymbolDef {
                        id: type_id(NodeKind::Class, &fqcn),
                        kind: NodeKind::Class,
                        fqcn: fqcn.clone(),
                        name: name.to_string(),
                        owner: None,
                        range: Range::default(),
                        modifiers: Vec::new(),
                        param_types: Vec::new(),
                        return_type: None,
                        declared_type: None,
                        stereotype: Some("service".into()),
                    },
                    method_def(&fqcn, "save", &["User"], None),
                ],
                imports: vec![],
                reference_sites: vec![heritage(&fqcn, "UserService", RefKind::Implements)],
                type_bindings: vec![],
                contract_sites: vec![],
            }
        };
        let caller = ParsedFile {
            file: "com/acme/OrderController.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                SymbolDef {
                    id: type_id(NodeKind::Class, "com.acme.OrderController"),
                    kind: NodeKind::Class,
                    fqcn: "com.acme.OrderController".into(),
                    name: "OrderController".into(),
                    owner: None,
                    range: Range::default(),
                    modifiers: Vec::new(),
                    param_types: Vec::new(),
                    return_type: None,
                    declared_type: None,
                    stereotype: Some("controller".into()),
                },
                method_def("com.acme.OrderController", "placeOrder", &["Order"], None),
                field_def("com.acme.OrderController", "userService", "UserService"),
            ],
            imports: vec![],
            reference_sites: vec![ReferenceSite {
                name: "save".into(),
                receiver: Some("userService".into()),
                kind: RefKind::Call,
                arity: Some(1),
                range: Range::default(),
                in_fqcn: "com.acme.OrderController#placeOrder/1".into(),
                in_callable: method_id("com.acme.OrderController", "placeOrder", 1),
            }],
            type_bindings: vec![TypeBinding {
                name: "userService".into(),
                raw_type: "UserService".into(),
                kind: BindingKind::Field,
                in_fqcn: "com.acme.OrderController".into(),
                range: Range::default(),
            }],
            contract_sites: vec![],
        };
        let out = resolve_edges(&[
            iface,
            make_impl("UserServiceImplA"),
            make_impl("UserServiceImplB"),
            caller,
        ]);
        let calls: Vec<_> = out
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect();
        // Must fall back to interface — ambiguous, don't pick one
        assert!(
            calls
                .iter()
                .any(|e| e.dst == method_id("com.acme.UserService", "save", 1)),
            "ambiguous DI should fall back to interface method"
        );
        assert!(
            !calls.iter().any(|e| e.reason == "di-resolved"),
            "should not emit di-resolved edge when ambiguous"
        );
    }

    #[test]
    fn di_not_applied_to_concrete_class_receiver() {
        // Field typed as concrete class, not interface — DI should not change behavior
        let files = {
            let concrete = ParsedFile {
                file: "com/acme/UserServiceImpl.java".into(),
                package: Some("com.acme".into()),
                defs: vec![
                    SymbolDef {
                        id: type_id(NodeKind::Class, "com.acme.UserServiceImpl"),
                        kind: NodeKind::Class,
                        fqcn: "com.acme.UserServiceImpl".into(),
                        name: "UserServiceImpl".into(),
                        owner: None,
                        range: Range::default(),
                        modifiers: Vec::new(),
                        param_types: Vec::new(),
                        return_type: None,
                        declared_type: None,
                        stereotype: Some("service".into()),
                    },
                    method_def("com.acme.UserServiceImpl", "save", &["User"], None),
                ],
                imports: vec![],
                reference_sites: vec![],
                type_bindings: vec![],
                contract_sites: vec![],
            };
            let caller = ParsedFile {
                file: "com/acme/OrderController.java".into(),
                package: Some("com.acme".into()),
                defs: vec![
                    SymbolDef {
                        id: type_id(NodeKind::Class, "com.acme.OrderController"),
                        kind: NodeKind::Class,
                        fqcn: "com.acme.OrderController".into(),
                        name: "OrderController".into(),
                        owner: None,
                        range: Range::default(),
                        modifiers: Vec::new(),
                        param_types: Vec::new(),
                        return_type: None,
                        declared_type: None,
                        stereotype: Some("controller".into()),
                    },
                    method_def("com.acme.OrderController", "placeOrder", &["Order"], None),
                    field_def(
                        "com.acme.OrderController",
                        "userServiceImpl",
                        "UserServiceImpl",
                    ),
                ],
                imports: vec![],
                reference_sites: vec![ReferenceSite {
                    name: "save".into(),
                    receiver: Some("userServiceImpl".into()),
                    kind: RefKind::Call,
                    arity: Some(1),
                    range: Range::default(),
                    in_fqcn: "com.acme.OrderController#placeOrder/1".into(),
                    in_callable: method_id("com.acme.OrderController", "placeOrder", 1),
                }],
                type_bindings: vec![TypeBinding {
                    name: "userServiceImpl".into(),
                    raw_type: "UserServiceImpl".into(),
                    kind: BindingKind::Field,
                    in_fqcn: "com.acme.OrderController".into(),
                    range: Range::default(),
                }],
                contract_sites: vec![],
            };
            vec![concrete, caller]
        };
        let out = resolve_edges(&files);
        let calls: Vec<_> = out
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect();
        assert!(
            calls
                .iter()
                .any(|e| e.dst == method_id("com.acme.UserServiceImpl", "save", 1)),
            "concrete field should resolve directly to impl"
        );
        assert!(
            !calls.iter().any(|e| e.reason == "di-resolved"),
            "concrete field should use receiver-bound, not di-resolved"
        );
    }

    #[test]
    fn di_resolves_repository_interface() {
        // @Repository stereotype also qualifies for DI resolution
        let files = make_di_scenario(Some("repository"));
        let out = resolve_edges(&files);
        let calls: Vec<_> = out
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect();
        assert!(
            calls.iter().any(|e| {
                e.dst == method_id("com.acme.UserServiceImpl", "save", 1)
                    && e.reason == "di-resolved"
            }),
            "@Repository impl should also be DI-resolved"
        );
    }

    // ── UnresolvedRef diagnostics tests ──────────────────────────────────────

    #[test]
    fn unresolved_ref_receiver_type_unknown() {
        // Ref site with a receiver name that has no binding → receiver_type_unknown
        let file = ParsedFile {
            file: "com/acme/Foo.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                type_def(NodeKind::Class, "com.acme.Foo"),
                method_def("com.acme.Foo", "go", &[], None),
            ],
            imports: vec![],
            reference_sites: vec![ref_site(
                "com.acme.Foo#go/0",
                method_id("com.acme.Foo", "go", 0),
                RefKind::Call,
                Some("unknownReceiver"),
                "doSomething",
                Some(0),
            )],
            type_bindings: vec![],
            contract_sites: vec![],
        };
        let out = resolve_edges(&[file]);
        assert_eq!(out.skipped, 1);
        assert_eq!(out.unresolved_refs.len(), 1);
        let r = &out.unresolved_refs[0];
        assert_eq!(r.reason, "receiver_type_unknown");
        assert_eq!(r.name, "doSomething");
        assert_eq!(r.receiver.as_deref(), Some("unknownReceiver"));
        assert_eq!(r.file, "com/acme/Foo.java");
    }

    #[test]
    fn unresolved_ref_member_not_found() {
        // Receiver type resolves (field binding) but method absent → member_not_found
        let service = ParsedFile {
            file: "com/acme/MyService.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                type_def(NodeKind::Class, "com.acme.MyService"),
                method_def("com.acme.MyService", "knownMethod", &[], None),
            ],
            imports: vec![],
            reference_sites: vec![],
            type_bindings: vec![],
            contract_sites: vec![],
        };
        let caller = ParsedFile {
            file: "com/acme/Caller.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                type_def(NodeKind::Class, "com.acme.Caller"),
                method_def("com.acme.Caller", "run", &[], None),
                field_def("com.acme.Caller", "svc", "MyService"),
            ],
            imports: vec![],
            reference_sites: vec![ref_site(
                "com.acme.Caller#run/0",
                method_id("com.acme.Caller", "run", 0),
                RefKind::Call,
                Some("svc"),
                "missingMethod",
                Some(0),
            )],
            type_bindings: vec![TypeBinding {
                name: "svc".into(),
                raw_type: "MyService".into(),
                kind: BindingKind::Field,
                in_fqcn: "com.acme.Caller".into(),
                range: Range::default(),
            }],
            contract_sites: vec![],
        };
        let out = resolve_edges(&[service, caller]);
        assert_eq!(out.skipped, 1);
        let r = &out.unresolved_refs[0];
        assert_eq!(r.reason, "member_not_found");
        assert_eq!(
            r.resolved_receiver_type.as_deref(),
            Some("com.acme.MyService")
        );
    }

    #[test]
    fn unresolved_ref_heritage_type_unknown() {
        // Class extends a type not in the parsed scope → heritage_type_unknown
        let child = ParsedFile {
            file: "com/acme/Child.java".into(),
            package: Some("com.acme".into()),
            defs: vec![type_def(NodeKind::Class, "com.acme.Child")],
            imports: vec![],
            reference_sites: vec![heritage("com.acme.Child", "MissingParent", RefKind::Extends)],
            type_bindings: vec![],
            contract_sites: vec![],
        };
        let out = resolve_edges(&[child]);
        assert_eq!(out.skipped, 1);
        let r = &out.unresolved_refs[0];
        assert_eq!(r.reason, "heritage_type_unknown");
        assert_eq!(r.name, "MissingParent");
    }

    #[test]
    fn callresult_factory_pattern_resolved() {
        // var order = factory.create(); order.process()
        // factory field typed OrderFactory → create() returns Order → process() resolves
        let order_factory = ParsedFile {
            file: "com/acme/OrderFactory.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                type_def(NodeKind::Class, "com.acme.OrderFactory"),
                method_def("com.acme.OrderFactory", "create", &[], Some("Order")),
            ],
            imports: vec![],
            reference_sites: vec![],
            type_bindings: vec![],
            contract_sites: vec![],
        };
        let order = ParsedFile {
            file: "com/acme/Order.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                type_def(NodeKind::Class, "com.acme.Order"),
                method_def("com.acme.Order", "process", &[], None),
            ],
            imports: vec![],
            reference_sites: vec![],
            type_bindings: vec![],
            contract_sites: vec![],
        };
        let service = ParsedFile {
            file: "com/acme/OrderService.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                type_def(NodeKind::Class, "com.acme.OrderService"),
                method_def("com.acme.OrderService", "run", &[], None),
                field_def("com.acme.OrderService", "factory", "OrderFactory"),
            ],
            imports: vec![],
            // order.process() — receiver "order" has CallResult("create") binding
            reference_sites: vec![ref_site(
                "com.acme.OrderService#run/0",
                method_id("com.acme.OrderService", "run", 0),
                RefKind::Call,
                Some("order"),
                "process",
                Some(0),
            )],
            // var order = create();  raw_type = "create", kind = CallResult
            type_bindings: vec![
                TypeBinding {
                    name: "factory".into(),
                    raw_type: "OrderFactory".into(),
                    kind: BindingKind::Field,
                    in_fqcn: "com.acme.OrderService".into(),
                    range: Range::default(),
                },
                TypeBinding {
                    name: "order".into(),
                    raw_type: "create".into(),
                    kind: BindingKind::CallResult,
                    in_fqcn: "com.acme.OrderService#run/0".into(),
                    range: Range::default(),
                },
            ],
            contract_sites: vec![],
        };
        let out = resolve_edges(&[order_factory, order, service]);
        let calls: Vec<_> = out
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .collect();
        assert!(
            calls.iter().any(|e| e.dst == method_id("com.acme.Order", "process", 0)),
            "factory CallResult should resolve order.process() to Order#process/0"
        );
        assert_eq!(out.skipped, 0, "no unresolved refs when factory pattern works");
    }

    #[test]
    fn callresult_factory_pattern_unresolved_when_return_type_absent() {
        // var order = create(); — but create() has no return_type → unresolvable
        let service = ParsedFile {
            file: "com/acme/OrderService.java".into(),
            package: Some("com.acme".into()),
            defs: vec![
                type_def(NodeKind::Class, "com.acme.OrderService"),
                method_def("com.acme.OrderService", "run", &[], None),
                // no create() method, no fields that have it
            ],
            imports: vec![],
            reference_sites: vec![ref_site(
                "com.acme.OrderService#run/0",
                method_id("com.acme.OrderService", "run", 0),
                RefKind::Call,
                Some("order"),
                "process",
                Some(0),
            )],
            type_bindings: vec![TypeBinding {
                name: "order".into(),
                raw_type: "create".into(),
                kind: BindingKind::CallResult,
                in_fqcn: "com.acme.OrderService#run/0".into(),
                range: Range::default(),
            }],
            contract_sites: vec![],
        };
        let out = resolve_edges(&[service]);
        assert_eq!(out.skipped, 1);
        let r = &out.unresolved_refs[0];
        // Receiver "order" has a CallResult binding but return type can't be resolved;
        // the receiver ends up unresolvable → receiver_type_unknown
        assert_eq!(r.reason, "receiver_type_unknown");
    }
}
