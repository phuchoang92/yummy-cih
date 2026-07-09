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
    assert_eq!(
        props["route_annotations"],
        serde_json::json!(["GET", "Path"])
    );

    let post = routes.iter().find(|n| n.name == "POST /accounts").unwrap();
    assert_eq!(
        post.props.as_ref().unwrap()["route_annotations"],
        serde_json::json!(["POST"])
    );
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
    assert!(
        route_nodes(src).is_empty(),
        "expected no routes from method-level @RequestMapping"
    );
}

// ── Messaging / HTTP contract sites (ContractKind + messaging_framework) ─────

fn contract_sites(src: &str) -> Vec<cih_core::ContractSite> {
    JavaProvider::new()
        .parse_file("Sample.java", src)
        .expect("sample should parse")
        .parsed_file
        .contract_sites
}

#[test]
fn kafka_listener_is_event_listen_kafka() {
    let src = r#"
        package com.acme;
        class OrderConsumer {
            @KafkaListener(topics = "orders.created")
            public void onOrder(String msg) {}
        }
    "#;
    let sites = contract_sites(src);
    let site = sites
        .iter()
        .find(|s| s.topic.as_deref() == Some("orders.created"))
        .expect("kafka listener contract site");
    assert_eq!(site.kind, cih_core::ContractKind::EventListen);
    assert_eq!(
        site.messaging_framework,
        Some(cih_core::MessagingFramework::Kafka)
    );
}

#[test]
fn spring_event_listener_is_event_listen_spring() {
    let src = r#"
        package com.acme;
        class UserListener {
            @EventListener
            public void on(UserSaved event) {}
        }
    "#;
    let sites = contract_sites(src);
    let site = sites
        .iter()
        .find(|s| s.kind == cih_core::ContractKind::EventListen)
        .expect("spring @EventListener contract site");
    assert_eq!(site.topic.as_deref(), Some("UserSaved"));
    assert_eq!(
        site.messaging_framework,
        Some(cih_core::MessagingFramework::Spring)
    );
}

#[test]
fn kafka_template_send_is_event_publish_kafka() {
    let src = r#"
        package com.acme;
        class OrderPublisher {
            private KafkaTemplate<String, String> kafkaTemplate;
            public void publish() {
                kafkaTemplate.send("orders.created", "payload");
            }
        }
    "#;
    let sites = contract_sites(src);
    let site = sites
        .iter()
        .find(|s| s.kind == cih_core::ContractKind::EventPublish)
        .expect("KafkaTemplate.send contract site");
    assert_eq!(site.topic.as_deref(), Some("orders.created"));
    assert_eq!(
        site.messaging_framework,
        Some(cih_core::MessagingFramework::Kafka)
    );
}

#[test]
fn application_event_publisher_is_event_publish_spring() {
    let src = r#"
        package com.acme;
        class Notifier {
            private ApplicationEventPublisher publisher;
            public void go() {
                publisher.publishEvent(new UserSavedEvent());
            }
        }
    "#;
    let sites = contract_sites(src);
    let site = sites
        .iter()
        .find(|s| s.kind == cih_core::ContractKind::EventPublish)
        .expect("ApplicationEventPublisher.publishEvent contract site");
    assert_eq!(site.topic.as_deref(), Some("UserSavedEvent"));
    assert_eq!(
        site.messaging_framework,
        Some(cih_core::MessagingFramework::Spring)
    );
}

#[test]
fn http_contract_sites_have_no_messaging_framework() {
    let src = r#"
        package com.acme;
        class OrderClient {
            private RestTemplate restTemplate;
            public void call() {
                restTemplate.getForObject("http://svc/api/orders/1", String.class);
            }
        }
    "#;
    let sites = contract_sites(src);
    let site = sites
        .iter()
        .find(|s| s.kind == cih_core::ContractKind::HttpCall)
        .expect("RestTemplate HTTP contract site");
    assert_eq!(site.messaging_framework, None);
}

#[test]
fn retains_generic_annotation_metadata_on_methods() {
    let src = r#"
        package com.acme;
        class C {
            @BankEndpoint("/pay")
            @Audited(level = "high")
            public void pay() {}
        }
    "#;
    let unit = JavaProvider::new()
        .parse_file("C.java", src)
        .expect("parse");
    let method = unit
        .nodes
        .iter()
        .find(|n| n.kind == cih_core::NodeKind::Method)
        .expect("method");
    let anns = method
        .props
        .as_ref()
        .unwrap()
        .get("annotations")
        .expect("annotations prop");
    let arr = anns.as_array().unwrap();
    let be = arr
        .iter()
        .find(|a| a["name"] == "BankEndpoint")
        .expect("BankEndpoint");
    assert_eq!(be["attrs"]["value"], "/pay");
    let au = arr
        .iter()
        .find(|a| a["name"] == "Audited")
        .expect("Audited");
    assert_eq!(au["attrs"]["level"], "high");
}
