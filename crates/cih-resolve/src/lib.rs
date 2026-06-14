//! Phase 4.1 — resolution indexes for scope resolution.
//!
//! Loads the Phase-3 `ParsedFile` IR for a scope and builds read-only, cross-file
//! indexes the emit passes (Phase 4.2) query: a def/type registry, per-file import
//! tables, heritage adjacency, and a precedence-ordered scope-binding lookup that
//! turns a receiver name into a resolved FQCN. This crate only *builds and answers*
//! — it emits no edges (that is 4.2).

use std::collections::{HashMap, HashSet};

use cih_core::{BindingKind, NodeId, NodeKind, ParsedFile, RawImport, RefKind, SymbolDef, TypeBinding};

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
                if let Some(def) = overloads
                    .iter()
                    .find(|d| d.param_types.len() as u16 == a)
                {
                    return Some(def.id.clone());
                }
            }
            return overloads.first().map(|d| d.id.clone());
        }
        self.fields.get(&key).map(|d| d.id.clone())
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

    fn field_type_in_hierarchy(&self, owner_class: &str, name: &str) -> Option<String> {
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
        self.implementors.get(fqcn).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn is_known_type(&self, fqcn: &str) -> bool {
        self.types_by_fqcn.contains_key(fqcn)
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
    bindings
        .iter()
        .filter(|b| b.name == name)
        .max_by(|a, b| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{field_id, method_id, type_id, Range, ReferenceSite};

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
                field_def("com.acme.OwnerController", "service", "OwnerService"),
                method_def("com.acme.OwnerController", "handle", &["OwnerService"], None),
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
        vec![repo, service, controller]
    }

    #[test]
    fn resolve_type_uses_import_same_package_and_generics() {
        let idx = ResolveIndex::build(&workspace());
        let f = "com/acme/OwnerController.java";
        assert_eq!(idx.resolve_type("List", f).as_deref(), Some("java.util.List"));
        assert_eq!(idx.resolve_type("Thing", f).as_deref(), Some("com.other.Thing"));
        assert_eq!(idx.resolve_type("Owner", f).as_deref(), Some("com.acme.Owner")); // same package, known
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
        assert_eq!(idx.find_member("com.acme.OwnerService", "missing", Some(0)), None);
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
        assert_eq!(idx.find_member("com.acme.OwnerService", "findAll", Some(0)), None);
        assert_eq!(
            idx.find_member_in_hierarchy("com.acme.OwnerService", "findAll", Some(0)),
            Some(method_id("com.acme.Repo", "findAll", 0))
        );
    }
}
