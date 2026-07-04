use std::collections::BTreeSet;

use streaming_iterator::StreamingIterator;
use tree_sitter::QueryCursor;

use cih_lang::{java::JavaProvider, LanguageProvider, Stereotype};

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
    assert!(found.contains("import.statement"));
    assert!(found.contains("declaration.variable"));
    assert!(found.contains("type-binding.type"));
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

#[test]
fn scan_file_extracts_package_and_spring_framework() {
    let provider = JavaProvider::new();
    let java = r#"
        package com.acme.owner;
        import org.springframework.web.bind.annotation.GetMapping;
        @RestController
        class OwnerController {
          @GetMapping("/owners")
          String owners() { return ""; }
        }
    "#;
    let scan = provider.scan_file("OwnerController.java", java).unwrap();
    assert_eq!(scan.package.as_deref(), Some("com.acme.owner"));
    assert!(scan.frameworks.contains("spring"));
    assert_eq!(scan.frameworks.len(), 1);
}

// ── Route heuristic edge cases ──────────────────────────────────────────────
// These pin the path-composition behavior that impact/route_map/taint depend on.

fn route_names(src: &str) -> BTreeSet<String> {
    route_nodes(src).into_iter().map(|n| n.name).collect()
}

#[test]
fn class_prefix_and_method_path_slashes_are_normalized() {
    // Trailing slash on the class prefix + leading slash on the method path must
    // collapse to a single separator, not `/owners//{id}`.
    let src = r#"
        package com.example;
        @RestController
        @RequestMapping("/owners/")
        class OwnerController {
          @GetMapping("/{id}")
          Object findOwner(Long id) { return null; }
        }
    "#;
    let names = route_names(src);
    assert!(names.contains("GET /owners/{id}"), "names={names:?}");
}

#[test]
fn method_annotation_without_path_inherits_class_prefix() {
    // A bare @GetMapping under a class @RequestMapping resolves to the prefix alone.
    let src = r#"
        package com.example;
        @RestController
        @RequestMapping("/owners")
        class OwnerController {
          @GetMapping
          Object all() { return null; }
        }
    "#;
    let names = route_names(src);
    assert!(names.contains("GET /owners"), "names={names:?}");
}

#[test]
fn multiple_paths_in_one_annotation_emit_multiple_routes() {
    let src = r#"
        package com.example;
        @RestController
        class OwnerController {
          @GetMapping({"/owners", "/members"})
          Object all() { return null; }
        }
    "#;
    let names = route_names(src);
    assert!(names.contains("GET /owners"), "names={names:?}");
    assert!(names.contains("GET /members"), "names={names:?}");
}

#[test]
fn method_level_request_mapping_emits_no_route() {
    // KNOWN LIMITATION: only the five @*Mapping shortcuts are recognized as verbs.
    // A method annotated only with @RequestMapping(method = RequestMethod.POST)
    // produces no Route node. Documented in docs/ARCHITECTURE.md; pinned here so
    // the day it changes, this test flags it deliberately.
    let src = r#"
        package com.example;
        @RestController
        @RequestMapping("/owners")
        class OwnerController {
          @RequestMapping(method = RequestMethod.POST)
          Object create() { return null; }
        }
    "#;
    assert!(route_nodes(src).is_empty(), "expected no routes from method-level @RequestMapping");
}
