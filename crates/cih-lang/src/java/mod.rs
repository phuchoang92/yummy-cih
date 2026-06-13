use std::sync::{Mutex, MutexGuard};

use once_cell::sync::Lazy;
use tree_sitter::{Language, Node, Parser, Query};

use crate::{LanguageProvider, Stereotype};

pub const JAVA_SCOPE_QUERY: &str = include_str!("query.scm");

static QUERY: Lazy<Query> =
    Lazy::new(|| Query::new(&language(), JAVA_SCOPE_QUERY).expect("Java scope query must compile"));

static PARSER: Lazy<Mutex<Parser>> = Lazy::new(|| {
    let mut parser = Parser::new();
    parser
        .set_language(&language())
        .expect("Java parser language must load");
    Mutex::new(parser)
});

#[derive(Clone, Copy, Debug, Default)]
pub struct JavaProvider;

impl JavaProvider {
    pub fn new() -> Self {
        Self
    }

    pub fn parser(&self) -> MutexGuard<'static, Parser> {
        PARSER.lock().expect("Java parser mutex poisoned")
    }

    pub fn parse(&self, src: &str) -> Option<tree_sitter::Tree> {
        self.parser().parse(src, None)
    }
}

impl LanguageProvider for JavaProvider {
    fn language(&self) -> Language {
        language()
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[".java"]
    }

    fn scope_query(&self) -> &Query {
        &QUERY
    }

    fn package_of(&self, root: Node<'_>, src: &str) -> Option<String> {
        package_of(root, src)
    }

    fn stereotype(&self, def_text: &str) -> Option<Stereotype> {
        stereotype(def_text)
    }
}

fn language() -> Language {
    tree_sitter_java::LANGUAGE.into()
}

fn package_of(root: Node<'_>, src: &str) -> Option<String> {
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
mod tests {
    use std::collections::BTreeSet;

    use streaming_iterator::StreamingIterator;
    use tree_sitter::QueryCursor;

    use super::*;

    const SAMPLE: &str = r#"
package com.example;

import java.util.List;

@RestController
class OwnerController {
    private OwnerService service;

    public Owner findOwner(Long id) {
        return service.findOwner(id);
    }
}
"#;

    #[test]
    fn parses_java_and_extracts_package() {
        let provider = JavaProvider::new();
        let tree = provider.parse(SAMPLE).expect("sample Java should parse");
        assert!(!tree.root_node().has_error());
        assert_eq!(
            provider.package_of(tree.root_node(), SAMPLE).as_deref(),
            Some("com.example")
        );
    }

    #[test]
    fn scope_query_captures_declarations_and_references() {
        let provider = JavaProvider::new();
        let tree = provider.parse(SAMPLE).expect("sample Java should parse");
        let query = provider.scope_query();
        let capture_names = query.capture_names();
        let mut cursor = QueryCursor::new();
        let mut found = BTreeSet::new();

        let mut matches = cursor.matches(query, tree.root_node(), SAMPLE.as_bytes());
        while let Some(query_match) = matches.next() {
            for capture in query_match.captures {
                found.insert(capture_names[capture.index as usize].to_string());
            }
        }

        assert!(found.contains("declaration.class"));
        assert!(found.contains("declaration.method"));
        assert!(found.iter().any(|name| name.starts_with("reference.call.")));
    }

    #[test]
    fn stereotype_detects_java_framework_annotations() {
        let provider = JavaProvider::new();
        assert_eq!(
            provider.stereotype("@RestController class OwnerController {}"),
            Some(Stereotype::Spring)
        );
        assert_eq!(
            provider.stereotype("@Path(\"/owners\") class OwnerResource {}"),
            Some(Stereotype::JaxRs)
        );
        assert_eq!(provider.stereotype("class Plain {}"), None);
    }
}
