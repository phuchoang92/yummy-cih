use std::path::Path;

use cih_core::{Edge, Node, ParsedFile, SymbolDef};

use crate::common::index::CommonIndex;
use crate::lang::{InheritanceModel, LanguageResolver};
use crate::types::class_of;

pub mod di;

pub struct JavaResolver;

impl LanguageResolver for JavaResolver {
    fn language_id(&self) -> &'static str {
        "java"
    }

    fn constructor_name(&self) -> Option<&'static str> {
        Some("<init>")
    }

    fn is_self_receiver(&self, name: &str) -> bool {
        matches!(name, "this" | "super")
    }

    fn resolve_self_receiver(
        &self,
        keyword: &str,
        in_fqcn: &str,
        index: &CommonIndex,
    ) -> Option<String> {
        match keyword {
            "this" => Some(class_of(in_fqcn).to_string()),
            "super" => index.supertypes(class_of(in_fqcn)).first().cloned(),
            _ => None,
        }
    }

    fn di_redirect(&self, type_qname: &str, index: &CommonIndex) -> Option<String> {
        // Prefer Spring-annotated bean (requires stereotype metadata from DI XML or annotations).
        if let Some(bean) = di::single_bean_impl(type_qname, index) {
            return Some(bean);
        }
        // Fallback: single concrete implementor in the workspace (annotation-driven wiring).
        index.single_programmatic_impl(type_qname, "java").map(str::to_string)
    }

    fn type_metadata(&self, def: &SymbolDef) -> Option<String> {
        def.stereotype.clone()
    }

    fn inheritance_model(&self) -> InheritanceModel {
        InheritanceModel::JavaNominal
    }

    fn extra_edges(&self, repo_root: Option<&Path>, parsed: &[ParsedFile]) -> (Vec<Node>, Vec<Edge>) {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        if let Some(root) = repo_root {
            let result = crate::di_xml::extract_di_xml(root, parsed);
            nodes.extend(result.nodes);
            edges.extend(result.edges);
        }
        (nodes, edges)
    }
}
