use std::collections::{HashMap, HashSet};

use cih_core::{
    BindingKind, NodeId, NodeKind, ParsedFile, RawImport, RefKind, SymbolDef, TypeBinding,
};

use crate::types::{base_type_name, class_of, is_type_kind, pick_binding, simple_of, stable_dedup};

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
                    if let Some(s) = def.framework_role.as_deref() {
                        idx.type_stereotypes.insert(def.fqcn.clone(), s.to_string());
                    }
                    idx.types_by_fqcn.insert(def.fqcn.clone(), def.clone());
                    idx.simple_to_fqcns
                        .entry(simple_of(&def.fqcn))
                        .or_default()
                        .push(def.fqcn.clone());
                    idx.file_of_type.insert(def.fqcn.clone(), pf.file.clone());
                } else if matches!(
                    def.kind,
                    NodeKind::Method | NodeKind::Constructor | NodeKind::Function
                ) {
                    // `Function` covers free functions and (in dynamic languages like Python/TS that
                    // don't distinguish) class methods, keyed by (container fqcn, name) so
                    // `find_member` resolves them. Mirrors `common/index.rs`.
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

    pub(crate) fn all_methods(&self) -> &HashMap<(String, String), Vec<SymbolDef>> {
        &self.methods
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
    pub(crate) fn di_impl(&self, interface_fqcn: &str) -> Option<String> {
        let impls = self.implementors.get(interface_fqcn)?;
        let beans: Vec<&String> = impls.iter().filter(|f| self.is_spring_bean(f)).collect();
        if beans.len() == 1 {
            Some(beans[0].clone())
        } else {
            None
        }
    }
}
