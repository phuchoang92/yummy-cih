use std::cell::RefCell;
use std::collections::BTreeSet;

use once_cell::sync::Lazy;
use tree_sitter::{Language, Node as TsNode, Parser, Query};

use crate::{LanguageProvider, SourceScan, Stereotype};

mod framework;
pub mod parse;

pub const GO_SCOPE_QUERY: &str = include_str!("query.scm");

static QUERY: Lazy<Query> =
    Lazy::new(|| Query::new(&language(), GO_SCOPE_QUERY).expect("Go scope query must compile"));

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(make_parser());
}

fn language() -> Language {
    tree_sitter_go::LANGUAGE.into()
}

pub fn make_parser() -> Parser {
    let mut p = Parser::new();
    p.set_language(&language()).expect("Go parser must load");
    p
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GoProvider;

impl GoProvider {
    pub fn new() -> Self {
        Self
    }
}

impl LanguageProvider for GoProvider {
    fn language(&self) -> Language {
        language()
    }

    fn language_id(&self) -> &'static str {
        "go"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[".go"]
    }

    fn scope_query(&self) -> &Query {
        &QUERY
    }

    fn package_of(&self, root: TsNode<'_>, src: &str) -> Option<String> {
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            if child.kind() == "package_clause" {
                let mut ic = child.walk();
                for c in child.named_children(&mut ic) {
                    if c.kind() == "package_identifier" {
                        return Some(
                            c.utf8_text(src.as_bytes()).unwrap_or("").trim().to_string(),
                        );
                    }
                }
            }
        }
        None
    }

    fn stereotype(&self, _def_text: &str) -> Option<Stereotype> {
        None
    }

    fn parse_file(&self, rel: &str, src: &str) -> anyhow::Result<cih_core::ParsedUnit> {
        parse::parse_go_file(rel, src)
    }

    fn scan_file(&self, _rel: &str, src: &str) -> anyhow::Result<SourceScan> {
        let loc = src.bytes().filter(|b| *b == b'\n').count() as u64;
        let mut frameworks = BTreeSet::new();
        if src.contains("\"net/http\"") || src.contains("`net/http`") {
            frameworks.insert("go-http".into());
        }
        if src.contains("gin.") || src.contains("\"github.com/gin-gonic/gin\"") {
            frameworks.insert("gin".into());
        }
        if src.contains("echo.") || src.contains("\"github.com/labstack/echo") {
            frameworks.insert("echo".into());
        }
        Ok(SourceScan {
            loc,
            package: None,
            frameworks,
        })
    }
}
