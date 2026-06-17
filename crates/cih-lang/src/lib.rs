pub mod java;

pub trait LanguageProvider: Send + Sync {
    fn language(&self) -> tree_sitter::Language;
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
}
