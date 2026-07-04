use std::cell::RefCell;
use std::collections::BTreeSet;

use once_cell::sync::Lazy;
use tree_sitter::{Language, Node as TsNode, Parser, Query};

use crate::{LanguageProvider, SourceScan, Stereotype};

pub mod parse;

pub const SCALA_SCOPE_QUERY: &str = include_str!("query.scm");

static QUERY: Lazy<Query> =
    Lazy::new(|| Query::new(&language(), SCALA_SCOPE_QUERY).expect("Scala scope query must compile"));

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(make_parser());
}

fn language() -> Language {
    tree_sitter_scala::LANGUAGE.into()
}

pub fn make_parser() -> Parser {
    let mut p = Parser::new();
    p.set_language(&language()).expect("Scala parser must load");
    p
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScalaProvider;

impl ScalaProvider {
    pub fn new() -> Self { Self }
}

impl LanguageProvider for ScalaProvider {
    fn language(&self) -> Language { language() }
    fn language_id(&self) -> &'static str { "scala" }
    fn extensions(&self) -> &'static [&'static str] { &[".scala", ".sc"] }
    fn scope_query(&self) -> &Query { &QUERY }
    fn package_of(&self, _root: TsNode<'_>, _src: &str) -> Option<String> { None }
    fn stereotype(&self, _def_text: &str) -> Option<Stereotype> { None }

    fn parse_file(&self, rel: &str, src: &str) -> anyhow::Result<cih_core::ParsedUnit> {
        parse::parse_scala_file(rel, src)
    }

    fn scan_file(&self, _rel: &str, src: &str) -> anyhow::Result<SourceScan> {
        let loc = src.bytes().filter(|b| *b == b'\n').count() as u64;
        let mut frameworks = BTreeSet::new();
        if src.contains("akka") || src.contains("Akka") {
            frameworks.insert("akka".into());
        }
        if src.contains("play.api") || src.contains("Play") {
            frameworks.insert("play".into());
        }
        Ok(SourceScan { loc, package: None, frameworks })
    }
}
