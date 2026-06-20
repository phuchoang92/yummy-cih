use std::cell::RefCell;

use once_cell::sync::Lazy;
use tree_sitter::{Language, Node as TsNode, Parser, Query};

use crate::{LanguageProvider, Stereotype};

mod parse;

pub const JAVA_SCOPE_QUERY: &str = include_str!("query.scm");

static QUERY: Lazy<Query> =
    Lazy::new(|| Query::new(&language(), JAVA_SCOPE_QUERY).expect("Java scope query must compile"));

thread_local! {
    // `tree_sitter::Parser` is `Send` but not `Sync`, so it cannot be shared across
    // threads. Each thread (e.g. each rayon worker in `cih-parse`) gets its own
    // parser, reused across the files it processes — no lock, no per-file rebuild.
    static PARSER: RefCell<Parser> = RefCell::new(make_parser());
}

/// Build a fresh Java parser. Callers that want to own a parser per task
/// (instead of using the thread-local `parse`) can use this directly.
pub fn make_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&language())
        .expect("Java parser language must load");
    parser
}

#[derive(Clone, Copy, Debug, Default)]
pub struct JavaProvider;

impl JavaProvider {
    pub fn new() -> Self {
        Self
    }

    pub fn parse(&self, src: &str) -> Option<tree_sitter::Tree> {
        PARSER.with(|parser| parser.borrow_mut().parse(src, None))
    }
}

impl LanguageProvider for JavaProvider {
    fn language(&self) -> Language {
        language()
    }

    fn language_id(&self) -> &'static str {
        "java"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[".java"]
    }

    fn scope_query(&self) -> &Query {
        &QUERY
    }

    fn package_of(&self, root: TsNode<'_>, src: &str) -> Option<String> {
        package_of(root, src)
    }

    fn stereotype(&self, def_text: &str) -> Option<Stereotype> {
        stereotype(def_text)
    }

    fn parse_file(&self, rel: &str, src: &str) -> anyhow::Result<cih_core::ParsedUnit> {
        parse::parse_java_file(self, rel, src)
    }
}

fn language() -> Language {
    tree_sitter_java::LANGUAGE.into()
}

fn package_of(root: TsNode<'_>, src: &str) -> Option<String> {
    let bytes = src.as_bytes();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "package_declaration" {
            continue;
        }
        let mut package_cursor = child.walk();
        for package_child in child.named_children(&mut package_cursor) {
            if package_child.kind() == "scoped_identifier" || package_child.kind() == "identifier" {
                let package = package_child.utf8_text(bytes).ok()?.trim();
                if !package.is_empty() {
                    return Some(package.to_string());
                }
            }
        }
    }
    None
}

fn stereotype(def_text: &str) -> Option<Stereotype> {
    if def_text.is_empty() {
        return None;
    }

    let normalized = def_text.to_ascii_lowercase();
    if [
        "@restcontroller",
        "@controller",
        "@getmapping",
        "@postmapping",
        "@requestmapping",
    ]
    .iter()
    .any(|pattern| normalized.contains(pattern))
    {
        return Some(Stereotype::Spring);
    }

    if ["@path", "@get", "@post", "@put", "@delete"]
        .iter()
        .any(|pattern| normalized.contains(pattern))
    {
        return Some(Stereotype::JaxRs);
    }

    None
}

#[cfg(test)]
mod tests;

