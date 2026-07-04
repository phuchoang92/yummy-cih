use std::cell::RefCell;
use std::collections::BTreeSet;

use once_cell::sync::Lazy;
use tree_sitter::{Language, Node as TsNode, Parser, Query};

use crate::{LanguageProvider, SourceScan, Stereotype};

pub mod parse;

pub const CS_SCOPE_QUERY: &str = include_str!("query.scm");

static QUERY: Lazy<Query> =
    Lazy::new(|| Query::new(&language(), CS_SCOPE_QUERY).expect("C# scope query must compile"));

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(make_parser());
}

fn language() -> Language {
    tree_sitter_c_sharp::LANGUAGE.into()
}

pub fn make_parser() -> Parser {
    let mut p = Parser::new();
    p.set_language(&language()).expect("C# parser must load");
    p
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CSharpProvider;

impl CSharpProvider {
    pub fn new() -> Self { Self }
}

impl LanguageProvider for CSharpProvider {
    fn language(&self) -> Language { language() }
    fn language_id(&self) -> &'static str { "csharp" }
    fn extensions(&self) -> &'static [&'static str] { &[".cs"] }
    fn scope_query(&self) -> &Query { &QUERY }
    fn package_of(&self, _root: TsNode<'_>, _src: &str) -> Option<String> { None }
    fn stereotype(&self, _def_text: &str) -> Option<Stereotype> { None }

    fn parse_file(&self, rel: &str, src: &str) -> anyhow::Result<cih_core::ParsedUnit> {
        parse::parse_csharp_file(rel, src)
    }

    fn scan_file(&self, _rel: &str, src: &str) -> anyhow::Result<SourceScan> {
        let loc = src.bytes().filter(|b| *b == b'\n').count() as u64;
        let mut frameworks = BTreeSet::new();
        if src.contains("Microsoft.AspNetCore") || src.contains("[ApiController]") {
            frameworks.insert("aspnet".into());
        }
        if src.contains("EntityFramework") || src.contains("DbContext") {
            frameworks.insert("ef-core".into());
        }
        Ok(SourceScan { loc, package: None, frameworks })
    }
}
