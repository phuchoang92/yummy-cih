//! The Python `Builder` — accumulates nodes/edges/references/contracts as the
//! walker visits each `def`/`class`/`import`. `parse.rs` drives it; the HTTP
//! detectors call into it.

use std::collections::HashMap;

use cih_core::{
    file_id, function_id, type_id, ContractSite, Edge, EdgeKind, HttpWrapperDef,
    Node, NodeId, NodeKind, RawImport, RefKind, ReferenceSite, RouteSource, StringConstant,
    SymbolDef,
};
use tree_sitter::Node as TsNode;

use crate::fingerprint::{compute_body_fingerprint, normalize_leaf_token_python};

use super::helpers::*;

// ── Builder ───────────────────────────────────────────────────────────────────

#[derive(Default)]
pub(super) struct Builder {
    pub(super) rel: String,
    pub(super) module: String,
    pub(super) nodes: Vec<Node>,
    pub(super) edges: Vec<Edge>,
    pub(super) defs: Vec<SymbolDef>,
    pub(super) imports: Vec<RawImport>,
    pub(super) reference_sites: Vec<ReferenceSite>,
    pub(super) contract_sites: Vec<ContractSite>,
    /// variable_name → url_prefix for FastAPI APIRouter instances
    pub(super) fastapi_prefixes: HashMap<String, String>,
    /// variable_name → url_prefix for Flask Blueprint instances
    pub(super) flask_prefixes: HashMap<String, String>,
    /// Whether the file imports FastAPI / Flask — disambiguates `@app.get`-style decorators, which
    /// are valid in both frameworks (mirrors `python/mod.rs` `detect_frameworks`).
    pub(super) has_fastapi: bool,
    pub(super) has_flask: bool,
    pub(super) string_constants: Vec<StringConstant>,
    pub(super) http_wrappers: Vec<HttpWrapperDef>,
}

impl Builder {
    pub(super) fn emit_class(&mut self, node: TsNode<'_>, _src: &str, name: &str, owner_fqn: Option<&str>) -> String {
        let fqn = if let Some(owner) = owner_fqn {
            format!("{owner}.{name}")
        } else {
            format!("{}.{}", self.module, name)
        };
        let id = type_id(NodeKind::Class, &fqn);
        let range = range_of(node);

        self.nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Class,
            name: name.to_string(),
            qualified_name: Some(fqn.clone()),
            file: self.rel.clone(),
            range,
            props: None,
        });
        let owner_id = owner_fqn.map(|f| type_id(NodeKind::Class, f));
        if let Some(ref oid) = owner_id {
            self.edges.push(Edge {
                src: oid.clone(),
                dst: id.clone(),
                kind: EdgeKind::Contains,
                confidence: 1.0,
                reason: "nested-class".into(),
            props: None,
            });
        } else {
            self.edges.push(Edge {
                src: file_id(&self.rel),
                dst: id.clone(),
                kind: EdgeKind::Contains,
                confidence: 1.0,
                reason: "file-type".into(),
            props: None,
            });
        }
        self.defs.push(SymbolDef {
            id,
            kind: NodeKind::Class,
            fqcn: fqn.clone(),
            name: name.to_string(),
            owner: owner_id,
            range,
            modifiers: Vec::new(),
            param_types: Vec::new(),
            return_type: None,
            declared_type: None,
            framework_role: None,
            complexity: None,
            body_fingerprint: None,
        lang_meta: None,
        });
        fqn
    }

    pub(super) fn emit_function(
        &mut self,
        node: TsNode<'_>,
        src: &str,
        name: &str,
        arity: u16,
        owner_fqn: Option<&str>,
    ) -> NodeId {
        let _ = src; // retained for API consistency
        let container_fqn = owner_fqn.unwrap_or(&self.module);
        let id = function_id(container_fqn, name, arity);
        let range = range_of(node);
        let owner_id = owner_fqn.map(|f| type_id(NodeKind::Class, f));

        self.nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Function,
            name: name.to_string(),
            qualified_name: Some(format!("{container_fqn}#{name}/{arity}")),
            file: self.rel.clone(),
            range,
            props: None,
        });

        if let Some(ref oid) = owner_id {
            self.edges.push(Edge {
                src: oid.clone(),
                dst: id.clone(),
                kind: EdgeKind::HasMethod,
                confidence: 1.0,
                reason: "member".into(),
            props: None,
            });
        } else {
            self.edges.push(Edge {
                src: file_id(&self.rel),
                dst: id.clone(),
                kind: EdgeKind::Contains,
                confidence: 1.0,
                reason: "file-fn".into(),
            props: None,
            });
        }

        let body_fingerprint = node
            .child_by_field_name("body")
            .and_then(|b| compute_body_fingerprint(b, "python", normalize_leaf_token_python));
        self.defs.push(SymbolDef {
            id: id.clone(),
            kind: NodeKind::Function,
            fqcn: container_fqn.to_string(),
            name: name.to_string(),
            owner: owner_id,
            range,
            modifiers: Vec::new(),
            param_types: Vec::new(),
            return_type: None,
            declared_type: None,
            framework_role: None,
            complexity: None,
            body_fingerprint,
            lang_meta: None,
        });
        id
    }

    pub(super) fn emit_flask_route(
        &mut self,
        fn_node: TsNode<'_>,
        fn_id: &NodeId,
        http_method: &str,
        path: &str,
    ) {
        let route_id = NodeId::new(format!("Route:flask:{}:{}", http_method, path));
        let name = format!("{http_method} {path}");
        self.nodes.push(Node {
            id: route_id.clone(),
            kind: NodeKind::Route,
            name: name.clone(),
            qualified_name: Some(name),
            file: self.rel.clone(),
            range: range_of(fn_node),
            props: Some(serde_json::json!({
                "httpMethod": http_method,
                "path": path,
                "route_annotations": [],
                "source": RouteSource::Flask,
                "handler": fn_id.as_str(),
            })),
        });
        self.edges.push(Edge {
            src: fn_id.clone(),
            dst: route_id,
            kind: EdgeKind::HandlesRoute,
            confidence: 1.0,
            reason: format!("flask-{}", http_method.to_ascii_lowercase()),
            props: None,
        });
    }

    pub(super) fn emit_fastapi_route(
        &mut self,
        fn_node: TsNode<'_>,
        fn_id: &NodeId,
        http_method: &str,
        path: &str,
    ) {
        let route_id = NodeId::new(format!("Route:fastapi:{}:{}", http_method, path));
        let name = format!("{http_method} {path}");
        self.nodes.push(Node {
            id: route_id.clone(),
            kind: NodeKind::Route,
            name: name.clone(),
            qualified_name: Some(name),
            file: self.rel.clone(),
            range: range_of(fn_node),
            props: Some(serde_json::json!({
                "httpMethod": http_method,
                "path": path,
                "route_annotations": [],
                "source": RouteSource::FastApi,
                "handler": fn_id.as_str(),
            })),
        });
        self.edges.push(Edge {
            src: fn_id.clone(),
            dst: route_id,
            kind: EdgeKind::HandlesRoute,
            confidence: 1.0,
            reason: format!("fastapi-{}", http_method.to_ascii_lowercase()),
            props: None,
        });
    }

    pub(super) fn emit_import(&mut self, node: TsNode<'_>, src: &str) {
        // Record DOTTED MODULE paths (`services.api_client`), not statement
        // text — the module string is the cross-file owner key the constant
        // resolver and wrapper index look up. Relative imports normalize
        // against this file's directory; un-normalizable forms record the
        // node text as-is (lookups miss — degrade, never guess).
        let range = range_of(node);
        let mut raws: Vec<(String, bool, Option<String>)> = Vec::new();
        match node.kind() {
            // `import a.b`, `import a.b as c`, `import os, sys` — one entry
            // per imported module.
            "import_statement" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    match child.kind() {
                        "dotted_name" => raws.push((text(child, src), false, None)),
                        "aliased_import" => {
                            if let Some(name) = child.child_by_field_name("name") {
                                let alias = child
                                    .child_by_field_name("alias")
                                    .map(|alias| text(alias, src));
                                raws.push((text(name, src), false, alias));
                            }
                        }
                        _ => {}
                    }
                }
            }
            // `from a.b import x, y` / `from a.b import *` / `from .x import y`
            // — ONE entry: the source module.
            "import_from_statement" => {
                let mut cursor = node.walk();
                let is_wildcard = node
                    .named_children(&mut cursor)
                    .any(|child| child.kind() == "wildcard_import");
                drop(cursor);
                let raw = match node.child_by_field_name("module_name") {
                    Some(module) if module.kind() == "dotted_name" => text(module, src),
                    Some(module) if module.kind() == "relative_import" => {
                        normalize_relative_import(&text(module, src), &self.rel)
                            .unwrap_or_else(|| text(node, src))
                    }
                    _ => text(node, src),
                };
                raws.push((raw, is_wildcard, None));
            }
            _ => raws.push((text(node, src), false, None)),
        }
        if raws.is_empty() {
            raws.push((text(node, src), false, None));
        }
        for (raw, is_wildcard, alias) in raws {
            self.imports.push(RawImport {
                raw,
                is_static: false,
                is_wildcard,
                alias,
                range,
            });
        }
    }

    /// `enclosing` is the function that lexically contains this call — its node id and its
    /// signature fqcn (`container#name/arity`). When present, the call is attributed to that
    /// function so a `Calls` edge originates from the caller; module-level calls (no enclosing
    /// function) fall back to the file / module, as before.
    pub(super) fn emit_call_reference(
        &mut self,
        node: TsNode<'_>,
        src: &str,
        enclosing: Option<(&NodeId, &str)>,
    ) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        let (name, receiver) = match func.kind() {
            "attribute" => {
                let obj = func.child_by_field_name("object").map(|n| text(n, src));
                let attr = func
                    .child_by_field_name("attribute")
                    .map(|n| text(n, src))
                    .unwrap_or_default();
                (attr, obj)
            }
            "identifier" => (text(func, src), None),
            _ => return,
        };
        if name.is_empty() {
            return;
        }
        let (in_fqcn, in_callable) = match enclosing {
            Some((id, fqcn)) => (fqcn.to_string(), id.clone()),
            None => (self.module.clone(), file_id(&self.rel)),
        };
        self.reference_sites.push(ReferenceSite {
            name,
            receiver,
            kind: RefKind::Call,
            arity: call_arity(node),
            range: range_of(func),
            in_fqcn,
            in_callable,
            arg_texts: Vec::new(),
        });
    }
}


