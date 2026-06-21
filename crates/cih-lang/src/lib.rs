pub mod constant_resolver;
pub mod java;
pub mod python;
pub mod typescript;

pub use constant_resolver::{ConstantResolver, NullConstantResolver, ResolutionContext};

pub trait LanguageProvider: Send + Sync {
    fn language(&self) -> tree_sitter::Language;
    /// Short lowercase identifier for this language, e.g. `"java"`, `"typescript"`, `"python"`.
    fn language_id(&self) -> &'static str;
    fn extensions(&self) -> &'static [&'static str];
    fn scope_query(&self) -> &tree_sitter::Query;
    fn package_of(&self, root: tree_sitter::Node<'_>, src: &str) -> Option<String>;
    fn stereotype(&self, def_text: &str) -> Option<Stereotype>;
    fn parse_file(&self, rel: &str, src: &str) -> anyhow::Result<cih_core::ParsedUnit>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stereotype {
    Spring,
    JaxRs,
    NestJs,
    Flask,
    FastApi,
}
