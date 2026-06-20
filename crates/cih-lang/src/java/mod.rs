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
        // Captures the parse driver (cih-parse) actually consumes — guard against
        // a grammar/query drift silently dropping them.
        assert!(found.contains("import.statement"));
        assert!(found.contains("declaration.variable")); // `private OwnerService service;`
        assert!(found.contains("type-binding.type")); // field type binding
    }

    const SPRING_ROUTES: &str = r#"
package com.example;

@RestController
@RequestMapping("/owners")
class OwnerController {
    @GetMapping("/{id}")
    public Owner findOwner(Long id) { return null; }
}
"#;

    const JAXRS_ROUTES: &str = r#"
package com.example;

import javax.ws.rs.GET;
import javax.ws.rs.POST;
import javax.ws.rs.Path;

@Path("/accounts")
class AccountResource {
    @GET
    @Path("/{id}")
    public Account get(Long id) { return null; }

    @POST
    public void create(Account a) {}
}
"#;

    fn route_nodes(src: &str) -> Vec<cih_core::Node> {
        let provider = JavaProvider::new();
        let unit = provider
            .parse_file("Sample.java", src)
            .expect("sample should parse");
        unit.nodes
            .into_iter()
            .filter(|n| n.kind == cih_core::NodeKind::Route)
            .collect()
    }

    #[test]
    fn spring_mvc_routes_emit_route_annotations_and_source() {
        let routes = route_nodes(SPRING_ROUTES);
        let route = routes
            .iter()
            .find(|n| n.name == "GET /owners/{id}")
            .expect("spring route present");
        let props = route.props.as_ref().unwrap();
        assert_eq!(props["source"], "spring_mvc");
        assert_eq!(
            props["route_annotations"],
            serde_json::json!(["GetMapping"])
        );
        assert_eq!(props["path"], "/owners/{id}");
    }

    #[test]
    fn jaxrs_routes_extracted_with_path_prefix() {
        let routes = route_nodes(JAXRS_ROUTES);
        let names: BTreeSet<String> = routes.iter().map(|n| n.name.clone()).collect();
        assert!(names.contains("GET /accounts/{id}"), "names={names:?}");
        assert!(names.contains("POST /accounts"), "names={names:?}");

        let get = routes
            .iter()
            .find(|n| n.name == "GET /accounts/{id}")
            .unwrap();
        let props = get.props.as_ref().unwrap();
        assert_eq!(props["source"], "jax_rs");
        assert_eq!(props["route_annotations"], serde_json::json!(["GET", "Path"]));

        let post = routes.iter().find(|n| n.name == "POST /accounts").unwrap();
        assert_eq!(post.props.as_ref().unwrap()["route_annotations"], serde_json::json!(["POST"]));
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
