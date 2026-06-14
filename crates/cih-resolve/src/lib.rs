//! Phase 4.1/4.2 — resolution indexes and reference-site edge emission.
//!
//! Loads the Phase-3 `ParsedFile` IR for a scope and builds read-only, cross-file
//! indexes the emit passes query: a def/type registry, per-file import tables,
//! heritage adjacency, and a precedence-ordered scope-binding lookup that turns a
//! receiver name into a resolved FQCN. The public [`resolve_edges`] entrypoint runs
//! the Phase 4.2 pass order and emits graph edges.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use cih_core::{
    file_id, BindingKind, Edge, EdgeKind, NodeId, NodeKind, ParsedFile, RawImport, RefKind,
    ReferenceSite, SymbolDef, TypeBinding,
};

/// Result of turning unresolved [`ReferenceSite`](cih_core::ReferenceSite)s into
/// graph edges.
#[derive(Clone, Debug, Default)]
pub struct ResolveOutput {
    pub edges: Vec<Edge>,
    /// Reference/import sites that could not be resolved to an in-scope node.
    pub skipped: u64,
    /// Qualified external types discovered while trying to resolve calls/ctors.
    pub unresolved_external_fqcns: Vec<String>,
}

/// Run Phase 4.2 over all parsed files: receiver-bound calls, free calls,
/// remaining references, import edges, then heritage edges.
pub fn resolve_edges(parsed: &[ParsedFile]) -> ResolveOutput {
    let index = ResolveIndex::build(parsed);
    EdgeEmitter::new(parsed, index).run()
}

/// Cross-file resolution index over a parsed scope.
#[derive(Debug, Default)]
pub struct ResolveIndex {
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
}

#[derive(Debug, Default)]
struct FileContext {
    package: Option<String>,
    imports: Vec<RawImport>,
}

impl ResolveIndex {
    /// Build the index from all `ParsedFile`s in the scope.
    pub fn build(parsed: &[ParsedFile]) -> Self {
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
    pub fn resolve_type(&self, raw: &str, file: &str) -> Option<String> {
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
    pub fn find_member(&self, owner_fqcn: &str, name: &str, arity: Option<u16>) -> Option<NodeId> {
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

    pub fn find_constructor(&self, owner_fqcn: &str, arity: Option<u16>) -> Option<NodeId> {
        self.find_member(owner_fqcn, "<init>", arity)
    }

    /// Like [`find_member`], but walks `owner_fqcn` + its supertypes (BFS) — the
    /// inheritance/MRO-ish member resolution the receiver-bound pass needs.
    pub fn find_member_in_hierarchy(
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

    pub fn find_field_in_hierarchy(&self, owner_fqcn: &str, name: &str) -> Option<NodeId> {
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

    pub fn member_return_type_in_hierarchy(
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
    pub fn receiver_type(&self, in_fqcn: &str, receiver: &str) -> Option<String> {
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
            // `var x = m(...);` — raw_type is a method name; best-effort treat it as a
            // call on the enclosing class and follow its return type.
            BindingKind::CallResult => {
                self.method_return_type_in_hierarchy(owner_class, &tb.raw_type)
            }
        }
    }

    pub fn field_type_in_hierarchy(&self, owner_class: &str, name: &str) -> Option<String> {
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

    /// Resolve a raw type name against the file that declares `type_fqcn`.
    fn resolve_in_type(&self, raw: &str, type_fqcn: &str) -> Option<String> {
        match self.file_of_type.get(type_fqcn) {
            Some(file) => self.resolve_type(raw, file),
            None => self.resolve_type(raw, ""),
        }
    }

    // --- accessors (for 4.2 / 4.3) ---------------------------------------

    pub fn supertypes(&self, fqcn: &str) -> &[String] {
        self.supertypes.get(fqcn).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn implementors(&self, fqcn: &str) -> &[String] {
        self.implementors
            .get(fqcn)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn is_known_type(&self, fqcn: &str) -> bool {
        self.types_by_fqcn.contains_key(fqcn)
    }

    pub fn type_node_id(&self, fqcn: &str) -> Option<NodeId> {
        self.types_by_fqcn.get(fqcn).map(|def| def.id.clone())
    }

    pub fn file_of_type(&self, fqcn: &str) -> Option<&str> {
        self.file_of_type.get(fqcn).map(String::as_str)
    }

    /// Every type FQCN in the scope (for MRO / whole-graph passes).
    pub fn type_fqcns(&self) -> impl Iterator<Item = &str> {
        self.types_by_fqcn.keys().map(String::as_str)
    }

    fn dedup(&mut self) {
        for v in self.simple_to_fqcns.values_mut() {
            v.sort();
            v.dedup();
        }
        for v in self.supertypes.values_mut() {
            v.sort();
            v.dedup();
        }
        for v in self.implementors.values_mut() {
            v.sort();
            v.dedup();
        }
    }
}

struct EdgeEmitter<'a> {
    parsed: &'a [ParsedFile],
    index: ResolveIndex,
    handled: HashSet<(usize, usize)>,
    edges: Vec<Edge>,
    skipped: u64,
    unresolved_external_fqcns: BTreeSet<String>,
}

impl<'a> EdgeEmitter<'a> {
    fn new(parsed: &'a [ParsedFile], index: ResolveIndex) -> Self {
        Self {
            parsed,
            index,
            handled: HashSet::new(),
            edges: Vec::new(),
            skipped: 0,
            unresolved_external_fqcns: BTreeSet::new(),
        }
    }

    fn run(mut self) -> ResolveOutput {
        self.emit_receiver_bound_calls();
        self.emit_free_call_fallback();
        self.emit_references_via_lookup();
        self.emit_import_edges();
        self.emit_heritage_edges();
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
                    self.note_unresolved_site(pf, site);
                    self.skipped += 1;
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
                    self.note_unresolved_site(pf, site);
                    self.skipped += 1;
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
            if let Some(dst) = self
                .index
                .find_member_in_hierarchy(&owner, &site.name, site.arity)
            {
                let confidence = if receiver.contains('.') || receiver.contains('(') {
                    0.7
                } else {
                    1.0
                };
                return Some((dst, confidence, "receiver-bound".to_string()));
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

    fn note_unresolved_site(&mut self, pf: &ParsedFile, site: &ReferenceSite) {
        if let Some(receiver) = site.receiver.as_deref() {
            if let Some(fqcn) = self.resolve_receiver_expr_type(pf, site, receiver) {
                if fqcn.contains('.') && !self.index.is_known_type(&fqcn) {
                    self.unresolved_external_fqcns.insert(fqcn);
                }
            }
        } else if matches!(
            site.kind,
            RefKind::Ctor | RefKind::TypeRef | RefKind::Extends | RefKind::Implements
        ) {
            if let Some(fqcn) = self.index.resolve_type(&site.name, &pf.file) {
                if fqcn.contains('.') && !self.index.is_known_type(&fqcn) {
                    self.unresolved_external_fqcns.insert(fqcn);
                }
            }
        }
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
            edges,
            skipped: self.skipped,
            unresolved_external_fqcns: self.unresolved_external_fqcns.into_iter().collect(),
        }
    }
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
    use cih_core::{constructor_id, field_id, method_id, type_id, EdgeKind, Range, ReferenceSite};

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
        };
        let thing = ParsedFile {
            file: "com/other/Thing.java".into(),
            package: Some("com.other".into()),
            defs: vec![type_def(NodeKind::Class, "com.other.Thing")],
            imports: vec![],
            reference_sites: vec![],
            type_bindings: vec![],
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
}
