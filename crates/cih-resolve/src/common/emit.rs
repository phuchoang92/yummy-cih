use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use cih_core::{
    file_id, CallSiteRecord, Edge, EdgeKind, Node, NodeId, ParsedFile, RawImport, RefKind,
    ReferenceSite,
};
use cih_lang::constant_resolver::{ConstantResolver, NullConstantResolver, ResolutionContext};

use crate::common::index::CommonIndex;
use crate::common::inheritance::build_mro_map;
use crate::contracts::resolve_contract_edges;
use crate::lang::{InheritanceModel, ResolverRegistry};
use crate::types::{
    call_name, class_of, is_simple_ident, split_last_dot_outside_parens, starts_uppercase,
};
use crate::{ResolveOutput, UnresolvedRef};

pub struct EdgeEmitter<'a> {
    parsed: &'a [ParsedFile],
    index: CommonIndex,
    registry: &'a ResolverRegistry,
    /// Optional constant resolver for enriching CALLS edge call-site args (Gap 4/3).
    constant_resolver: Box<dyn ConstantResolver>,
    handled: HashSet<(usize, usize)>,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    skipped: u64,
    unresolved_external_fqcns: BTreeSet<String>,
    unresolved_refs: Vec<UnresolvedRef>,
    /// Memoize `index.resolve_type_with_confidence(raw, file)` — same (raw, file) within
    /// an emit run always yields the same result, so we pay the lookup cost at most once.
    /// Stores `(fqcn, confidence)` so callers that need either can share one cache.
    resolve_cache: HashMap<(String, String), Option<(String, f32)>>,
}

impl<'a> EdgeEmitter<'a> {
    pub fn new(parsed: &'a [ParsedFile], index: CommonIndex, registry: &'a ResolverRegistry) -> Self {
        Self {
            parsed,
            index,
            registry,
            constant_resolver: Box::new(NullConstantResolver),
            handled: HashSet::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
            skipped: 0,
            unresolved_external_fqcns: BTreeSet::new(),
            unresolved_refs: Vec::new(),
            resolve_cache: HashMap::new(),
        }
    }

    /// Cached resolution returning `(fqcn, confidence)`. The index is read-only during
    /// emit so no invalidation is needed.
    fn resolve_with_confidence_cached(&mut self, raw: &str, file: &str) -> Option<(String, f32)> {
        let key = (raw.to_string(), file.to_string());
        if let Some(cached) = self.resolve_cache.get(&key) {
            return cached.clone();
        }
        let result = self.index.resolve_type_with_confidence(raw, file);
        self.resolve_cache.insert(key, result.clone());
        result
    }

    /// Convenience wrapper when only the FQCN is needed.
    fn resolve_type_cached(&mut self, raw: &str, file: &str) -> Option<String> {
        self.resolve_with_confidence_cached(raw, file).map(|(fqcn, _)| fqcn)
    }

    /// Replace the default no-op constant resolver with a real one (Gap 4).
    pub fn with_constant_resolver(mut self, resolver: impl ConstantResolver + 'static) -> Self {
        self.constant_resolver = Box::new(resolver);
        self
    }

    /// Replace the default no-op constant resolver with an already-boxed one (Gap 4).
    pub fn with_constant_resolver_boxed(mut self, resolver: Box<dyn ConstantResolver>) -> Self {
        self.constant_resolver = resolver;
        self
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
                    .resolve_type_cached(&site.name, &pf.file)
                    .filter(|f| f.contains('.') && !self.index.is_known_type(f));
                ("ctor_type_unknown", None, ext)
            }
            RefKind::TypeRef => {
                let ext = self
                    .resolve_type_cached(&site.name, &pf.file)
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

    pub fn run(mut self) -> ResolveOutput {
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
        let parsed = self.parsed;
        for (file_idx, pf) in parsed.iter().enumerate() {
            for (site_idx, site) in pf.reference_sites.iter().enumerate() {
                if site.kind != RefKind::Call || site.receiver.is_none() {
                    continue;
                }
                if let Some((dst, confidence, reason)) = self.resolve_receiver_bound_call(pf, site)
                {
                    self.push_calls_edge(
                        site.in_callable.clone(),
                        dst,
                        confidence,
                        reason,
                        site,
                        pf,
                    );
                    self.handled.insert((file_idx, site_idx));
                }
            }
        }
    }

    fn emit_free_call_fallback(&mut self) {
        let parsed = self.parsed;
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
                    self.push_calls_edge(
                        site.in_callable.clone(),
                        dst,
                        0.70,
                        "free-call-fallback".to_string(),
                        site,
                        pf,
                    );
                    self.handled.insert((file_idx, site_idx));
                }
            }
        }
    }

    fn emit_references_via_lookup(&mut self) {
        let parsed = self.parsed;
        for (file_idx, pf) in parsed.iter().enumerate() {
            for (site_idx, site) in pf.reference_sites.iter().enumerate() {
                if self.handled.contains(&(file_idx, site_idx)) {
                    continue;
                }
                let resolved = match site.kind {
                    // Pass 1 (receiver-bound) and pass 2 (free-call) already tried every
                    // Call site. Any that reach here were unresolvable; don't retry.
                    RefKind::Call => None,
                    RefKind::Ctor => self.resolve_constructor(pf, site).map(|(dst, conf)| {
                        (
                            site.in_callable.clone(),
                            dst,
                            EdgeKind::Calls,
                            conf,
                            "constructor".to_string(),
                        )
                    }),
                    RefKind::FieldRead | RefKind::FieldWrite => {
                        self.resolve_field_access(pf, site).map(|dst| {
                            (
                                site.in_callable.clone(),
                                dst,
                                EdgeKind::Accesses,
                                1.0_f32,
                                match site.kind {
                                    RefKind::FieldRead => "field-read",
                                    _ => "field-write",
                                }
                                .to_string(),
                            )
                        })
                    }
                    RefKind::TypeRef => self.resolve_type_node(pf, &site.name).map(|(dst, conf)| {
                        (
                            site.in_callable.clone(),
                            dst,
                            EdgeKind::Uses,
                            conf,
                            "type-ref".to_string(),
                        )
                    }),
                    RefKind::Extends | RefKind::Implements => None,
                };

                if let Some((src, dst, kind, confidence, reason)) = resolved {
                    self.push_edge(src, dst, kind, confidence, reason);
                    self.handled.insert((file_idx, site_idx));
                } else if !matches!(site.kind, RefKind::Extends | RefKind::Implements) {
                    let (reason, recv_type, ext_fqcn) = self.classify_unresolved_ref(pf, site);
                    self.push_unresolved(pf, site, reason, recv_type, ext_fqcn);
                }
            }
        }
    }

    fn emit_import_edges(&mut self) {
        let parsed = self.parsed;
        for pf in parsed {
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
        let parsed = self.parsed;
        for pf in parsed {
            for site in &pf.reference_sites {
                let kind = match site.kind {
                    RefKind::Extends => EdgeKind::Extends,
                    RefKind::Implements => EdgeKind::Implements,
                    _ => continue,
                };
                let Some((dst, conf)) = self.resolve_type_node(pf, &site.name) else {
                    let ext = self
                        .resolve_type_cached(&site.name, &pf.file)
                        .filter(|f| f.contains('.') && !self.index.is_known_type(f));
                    self.push_unresolved(pf, site, "heritage_type_unknown", None, ext);
                    continue;
                };
                self.push_edge(
                    site.in_callable.clone(),
                    dst,
                    kind,
                    conf,
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
            .all_methods()
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
            // Only run MRO for types whose language resolver supports inheritance
            let lang = self.index.language_of(&owner_fqcn).unwrap_or("");
            let resolver = self.registry.for_language(lang);
            if resolver.inheritance_model() == InheritanceModel::None {
                continue;
            }

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

        // Language-aware self-receiver handling (scoped to avoid borrow overlap).
        let lang = effective_lang(pf);
        let self_recv = {
            let r = self.registry.for_language(lang);
            if r.is_self_receiver(receiver) {
                Some(r.resolve_self_receiver(receiver, &site.in_fqcn, &self.index))
            } else {
                None
            }
        };
        if let Some(owner_opt) = self_recv {
            if let Some(owner) = owner_opt {
                if let Some(dst) =
                    self.index.find_member_in_hierarchy(&owner, &site.name, site.arity)
                {
                    return Some((dst, 0.8, "self-receiver".to_string()));
                }
            }
            return None;
        }

        if let Some(owner) = self.resolve_receiver_expr_type(pf, site, receiver) {
            // DI redirect: interface receiver with exactly one @Service impl → use the impl.
            let effective_owner = if self.index.is_interface_type(&owner) {
                self.registry.for_language(lang)
                    .di_redirect(&owner, &self.index)
                    .unwrap_or_else(|| owner.clone())
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

    fn resolve_constructor(&mut self, pf: &ParsedFile, site: &ReferenceSite) -> Option<(NodeId, f32)> {
        let (fqcn, conf) = self.resolve_with_confidence_cached(&site.name, &pf.file)?;
        let ctor_name = self.registry.for_language(effective_lang(pf)).constructor_name();
        let result = if let Some(ctor_name) = ctor_name {
            self.index.find_member(&fqcn, ctor_name, site.arity)
        } else {
            self.index.find_member(&fqcn, &site.name, site.arity)
        };
        if result.is_none() {
            if fqcn.contains('.') && !self.index.is_known_type(&fqcn) {
                self.unresolved_external_fqcns.insert(fqcn);
            }
        }
        result.map(|id| (id, conf))
    }

    fn resolve_field_access(&mut self, pf: &ParsedFile, site: &ReferenceSite) -> Option<NodeId> {
        let owner = match site.receiver.as_deref() {
            Some(receiver) => self.resolve_receiver_expr_type(pf, site, receiver)?,
            None => class_of(&site.in_fqcn).to_string(),
        };
        self.index.find_field_in_hierarchy(&owner, &site.name)
    }

    fn resolve_type_node(&mut self, pf: &ParsedFile, raw: &str) -> Option<(NodeId, f32)> {
        let (fqcn, conf) = self.resolve_with_confidence_cached(raw, &pf.file)?;
        let id = self.index.type_node_id(&fqcn);
        if id.is_none() && fqcn.contains('.') {
            self.unresolved_external_fqcns.insert(fqcn);
        }
        id.map(|n| (n, conf))
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
            // Language-aware self-receiver (this/super/self/cls)
            if self.registry.for_language(effective_lang(pf)).is_self_receiver(receiver) {
                return self.index.receiver_type(&site.in_fqcn, receiver);
            }
            if starts_uppercase(receiver) {
                if let Some(fqcn) = self.resolve_type_cached(receiver, &pf.file) {
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

        if let Some(fqcn) = self.resolve_type_cached(receiver, &pf.file) {
            if self.index.is_known_type(&fqcn) {
                return Some(fqcn);
            }
        }

        if let Some((left, right)) = split_last_dot_outside_parens(receiver) {
            if starts_uppercase(left) {
                if let Some(fqcn) = self.resolve_type_cached(left, &pf.file) {
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
        const CONFIDENCE_FLOOR: f32 = 0.60;
        if src.as_str() == "Method:<unknown>" || dst.as_str().is_empty() {
            self.skipped += 1;
            return;
        }
        if confidence < CONFIDENCE_FLOOR {
            self.skipped += 1;
            return;
        }
        let strategy: Option<&str> = match (&kind, reason.as_str()) {
            (EdgeKind::Calls, "di-resolved")            => Some("di_xml"),
            (EdgeKind::Calls, "interface_single_impl")  => Some("iface_single"),
            (EdgeKind::Calls, "receiver-bound")         => Some("type_inferred"),
            (EdgeKind::Calls, "self-receiver")          => Some("self_recv"),
            (EdgeKind::Calls, "free-call-fallback")     => Some("free_call"),
            (EdgeKind::Implements, _)                   => Some("heritage"),
            (EdgeKind::Extends, _)                      => Some("heritage"),
            _ => None,
        };
        let props = strategy.map(|s| serde_json::json!({ "rs": s }));
        self.edges.push(Edge {
            src,
            dst,
            kind,
            confidence,
            reason,
            props,
        });
    }

    /// Push a CALLS edge with call-site arg texts resolved via the constant resolver (Gap 3).
    fn push_calls_edge(
        &mut self,
        src: NodeId,
        dst: NodeId,
        confidence: f32,
        reason: String,
        site: &ReferenceSite,
        pf: &ParsedFile,
    ) {
        if src.as_str() == "Method:<unknown>" || dst.as_str().is_empty() {
            self.skipped += 1;
            return;
        }
        let props = if site.arg_texts.is_empty() {
            None
        } else {
            let ctx = ResolutionContext {
                file: Path::new(&pf.file),
                owner_fqcn: &site.in_fqcn,
                imports: &pf.imports,
            };
            let resolved_args: Vec<String> = site
                .arg_texts
                .iter()
                .map(|arg| {
                    self.constant_resolver
                        .resolve(arg, &ctx)
                        .unwrap_or_else(|| arg.clone())
                })
                .collect();
            let record = CallSiteRecord {
                range: site.range,
                args: resolved_args,
            };
            Some(serde_json::json!({ "call_sites": [record] }))
        };
        self.edges.push(Edge {
            src,
            dst,
            kind: EdgeKind::Calls,
            confidence,
            reason,
            props,
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

/// Infer the effective language for a file, falling back to extension-based detection
/// when `ParsedFile::language` is empty (old parse-cache artifacts).
fn effective_lang(pf: &ParsedFile) -> &str {
    if !pf.language.is_empty() {
        return &pf.language;
    }
    cih_lang::lang_for_path(&pf.file)
}
