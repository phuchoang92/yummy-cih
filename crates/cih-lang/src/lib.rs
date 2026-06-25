use std::collections::BTreeSet;

pub mod constant_resolver;
pub mod fingerprint;
pub mod generic_parse;
pub mod java;
pub mod kotlin;
pub mod python;
pub mod typescript;
pub mod go;
pub mod rust_lang;
pub mod csharp;
pub mod ruby;
pub mod php;
pub mod cpp;
pub mod scala;
pub mod bash;
pub mod elixir;

pub use constant_resolver::{ConstantResolver, NullConstantResolver, ResolutionContext};

/// Lightweight per-file scan metadata (no tree-sitter parse).
/// Returned by [`LanguageProvider::scan_file`] during the scan phase.
#[derive(Clone, Debug, Default)]
pub struct SourceScan {
    /// Lines of code (newline count).
    pub loc: u64,
    /// Best-effort namespace / package declaration, if any.
    pub package: Option<String>,
    /// Framework identifiers detected from cheap string matching.
    /// Normalized to a fixed set: `"spring"`, `"nestjs"`, `"flask"`, `"fastapi"`.
    pub frameworks: BTreeSet<String>,
}

pub trait LanguageProvider: Send + Sync {
    fn language(&self) -> tree_sitter::Language;
    /// Short lowercase identifier for this language, e.g. `"java"`, `"typescript"`, `"python"`.
    fn language_id(&self) -> &'static str;
    fn extensions(&self) -> &'static [&'static str];
    fn scope_query(&self) -> &tree_sitter::Query;
    fn package_of(&self, root: tree_sitter::Node<'_>, src: &str) -> Option<String>;
    fn stereotype(&self, def_text: &str) -> Option<Stereotype>;
    fn parse_file(&self, rel: &str, src: &str) -> anyhow::Result<cih_core::ParsedUnit>;

    /// Cheap per-file scan: LOC, package/namespace, framework hints.
    /// Called during the scan phase — no tree-sitter, just string matching.
    fn scan_file(&self, rel: &str, src: &str) -> anyhow::Result<SourceScan>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stereotype {
    Spring,
    JaxRs,
    NestJs,
    Flask,
    FastApi,
}
