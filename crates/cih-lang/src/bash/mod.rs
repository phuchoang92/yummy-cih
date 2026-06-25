use std::cell::RefCell;
use std::collections::BTreeSet;

use once_cell::sync::Lazy;
use tree_sitter::{Language, Node as TsNode, Parser, Query};

use crate::{LanguageProvider, SourceScan, Stereotype};

pub mod parse;

pub const BASH_SCOPE_QUERY: &str = include_str!("query.scm");

static QUERY: Lazy<Query> =
    Lazy::new(|| Query::new(&language(), BASH_SCOPE_QUERY).expect("Bash scope query must compile"));

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(make_parser());
}

fn language() -> Language {
    tree_sitter_bash::LANGUAGE.into()
}

pub fn make_parser() -> Parser {
    let mut p = Parser::new();
    p.set_language(&language()).expect("Bash parser must load");
    p
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BashProvider;

impl BashProvider {
    pub fn new() -> Self { Self }
}

impl LanguageProvider for BashProvider {
    fn language(&self) -> Language { language() }
    fn language_id(&self) -> &'static str { "bash" }
    fn extensions(&self) -> &'static [&'static str] { &[".sh", ".bash", ".zsh"] }
    fn scope_query(&self) -> &Query { &QUERY }
    fn package_of(&self, _root: TsNode<'_>, _src: &str) -> Option<String> { None }
    fn stereotype(&self, _def_text: &str) -> Option<Stereotype> { None }

    fn parse_file(&self, rel: &str, src: &str) -> anyhow::Result<cih_core::ParsedUnit> {
        parse::parse_bash_file(rel, src)
    }

    fn scan_file(&self, _rel: &str, src: &str) -> anyhow::Result<SourceScan> {
        let loc = src.bytes().filter(|b| *b == b'\n').count() as u64;
        Ok(SourceScan { loc, package: None, frameworks: BTreeSet::new() })
    }
}
