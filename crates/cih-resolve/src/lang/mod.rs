use std::collections::HashMap;
use std::path::Path;

use cih_core::{Edge, ImportBinding, Node, ParsedFile, SymbolDef};

use crate::common::index::CommonIndex;

pub mod java;
pub mod python;
pub mod typescript;

/// Per-language resolution strategy. All methods have safe defaults (None/false/empty)
/// so a new language implementation starts minimal and opts into what it needs.
pub trait LanguageResolver: Send + Sync {
    fn language_id(&self) -> &'static str;

    /// Node name used for constructors in graph IDs.
    /// Java: `"<init>"` | Python: `"__init__"` | TypeScript: `"constructor"` | Go: `None`
    fn constructor_name(&self) -> Option<&'static str> {
        None
    }

    /// True when `name` is a language self-reference keyword (Java: "this"/"super",
    /// Python: "self"/"cls"). These get special receiver resolution.
    fn is_self_receiver(&self, name: &str) -> bool {
        let _ = name;
        false
    }

    /// Resolve a self-reference keyword to the enclosing type qualified name.
    fn resolve_self_receiver(
        &self,
        keyword: &str,
        in_fqcn: &str,
        index: &CommonIndex,
    ) -> Option<String> {
        let _ = (keyword, in_fqcn, index);
        None
    }

    /// IoC/DI redirect: for an interface/abstract type, return the unambiguous
    /// concrete impl if the framework can determine it (Spring @Service).
    /// Return None when not applicable or ambiguous.
    fn di_redirect(&self, type_qname: &str, index: &CommonIndex) -> Option<String> {
        let _ = (type_qname, index);
        None
    }

    /// Per-language opaque metadata for a type definition, stored in CommonIndex.
    /// Java: Spring stereotype string. Only the same LanguageResolver interprets it.
    fn type_metadata(&self, def: &SymbolDef) -> Option<String> {
        let _ = def;
        None
    }

    /// Inheritance model for this language.
    fn inheritance_model(&self) -> InheritanceModel {
        InheritanceModel::None
    }

    /// Additional edges emitted after common passes (DI XML wiring, struct embedding, etc.).
    fn extra_edges(
        &self,
        repo_root: Option<&Path>,
        parsed: &[ParsedFile],
    ) -> (Vec<Node>, Vec<Edge>) {
        let _ = (repo_root, parsed);
        (vec![], vec![])
    }

    /// Resolve a normalized import binding to the qualified name of the symbol it imports.
    /// Returns None if the import cannot be resolved within the workspace.
    fn resolve_import(
        &self,
        binding: &ImportBinding,
        from_file: &str,
        index: &CommonIndex,
    ) -> Option<String> {
        let _ = (binding, from_file, index);
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InheritanceModel {
    /// No inheritance (Go, plain C structs).
    None,
    /// Java nominal inheritance: explicit extends/implements, C3 linearization.
    JavaNominal,
    /// Python C3: multiple inheritance, C3 linearization, `__mro__`.
    PythonC3,
    /// TypeScript nominal: explicit extends/implements, single-class inheritance.
    TypeScriptNominal,
}

/// Dispatches to the right resolver by ParsedFile::language.
/// Falls back to NoOpResolver for unknown languages.
pub struct ResolverRegistry {
    resolvers: HashMap<&'static str, Box<dyn LanguageResolver>>,
}

impl ResolverRegistry {
    pub fn new() -> Self {
        Self {
            resolvers: HashMap::new(),
        }
    }

    pub fn register(&mut self, r: impl LanguageResolver + 'static) {
        self.resolvers.insert(r.language_id(), Box::new(r));
    }

    pub fn for_language(&self, language: &str) -> &dyn LanguageResolver {
        self.resolvers
            .get(language)
            .map(|r| r.as_ref())
            .unwrap_or(&NoOpResolver)
    }

    /// Invoke extra_edges for each resolver, grouping files by language.
    pub fn extra_edges(
        &self,
        repo_root: Option<&Path>,
        parsed: &[ParsedFile],
    ) -> (Vec<Node>, Vec<Edge>) {
        let mut all_nodes = Vec::new();
        let mut all_edges = Vec::new();
        // Group files by language
        let mut by_lang: HashMap<&str, Vec<&ParsedFile>> = HashMap::new();
        for pf in parsed {
            by_lang.entry(pf.language.as_str()).or_default().push(pf);
        }
        // Invoke resolvers in deterministic order
        let mut lang_ids: Vec<&str> = by_lang.keys().copied().collect();
        lang_ids.sort();
        for lang in lang_ids {
            let files: Vec<ParsedFile> = by_lang[lang].iter().map(|pf| (*pf).clone()).collect();
            let resolver = self.for_language(lang);
            let (nodes, edges) = resolver.extra_edges(repo_root, &files);
            all_nodes.extend(nodes);
            all_edges.extend(edges);
        }
        (all_nodes, all_edges)
    }
}

impl Default for ResolverRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Default no-op: structural edges only (no DI, no keyword receivers, no constructors).
pub struct NoOpResolver;

impl LanguageResolver for NoOpResolver {
    fn language_id(&self) -> &'static str {
        "noop"
    }
}
