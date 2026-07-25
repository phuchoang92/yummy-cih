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

// ── Dynamic-URL parts (Phase B: constants + concat → url_parts) ─────────────

#[test]
fn concat_url_yields_url_parts() {
    use cih_core::UrlPart;
    let src = r#"
        package com.acme;
        class Client {
            private final RestTemplate restTemplate;
            void fetch(String id) {
                restTemplate.getForObject(BASE + "/" + id, String.class);
            }
        }
    "#;
    let sites = contract_sites(src);
    assert_eq!(sites.len(), 1, "expected one site, got {sites:?}");
    let site = &sites[0];
    assert_eq!(site.url_template, None);
    assert_eq!(
        site.url_parts.as_deref(),
        Some(
            &[
                UrlPart::ConstRef("BASE".into()),
                UrlPart::Lit("/".into()),
                UrlPart::ConstRef("id".into()),
            ][..]
        )
    );
}

#[test]
fn qualified_constant_and_call_in_url_parts() {
    use cih_core::UrlPart;
    let src = r#"
        package com.acme;
        class Client {
            private final RestTemplate restTemplate;
            void fetch() {
                restTemplate.getForObject(Constants.BASE + suffix(), String.class);
            }
        }
    "#;
    let sites = contract_sites(src);
    assert_eq!(
        sites[0].url_parts.as_deref(),
        Some(&[UrlPart::ConstRef("Constants.BASE".into()), UrlPart::Dynamic][..])
    );
}

#[test]
fn literal_url_has_no_parts() {
    let src = r#"
        package com.acme;
        class Client {
            private final RestTemplate restTemplate;
            void fetch() {
                restTemplate.getForObject("/api/orders", String.class);
            }
        }
    "#;
    let sites = contract_sites(src);
    assert_eq!(sites[0].url_template.as_deref(), Some("/api/orders"));
    assert_eq!(sites[0].url_parts, None);
}

#[test]
fn dynamic_kafka_topic_yields_parts() {
    use cih_core::UrlPart;
    let src = r#"
        package com.acme;
        class Producer {
            private final KafkaTemplate kafkaTemplate;
            void send() {
                kafkaTemplate.send(TOPIC, "payload");
            }
        }
    "#;
    let sites = contract_sites(src);
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].topic, None);
    assert_eq!(
        sites[0].url_parts.as_deref(),
        Some(&[UrlPart::ConstRef("TOPIC".into())][..])
    );
}

/// Regression: a JPA `@Entity` whose 512-byte class-props header window ends
/// inside a multibyte UTF-8 char must not panic — it previously sliced at a
/// non-char-boundary, and under the rayon per-file parse that aborts the whole
/// analyze. The `@Table` name is still extracted from the clamped window.
#[test]
fn entity_header_slice_clamps_to_char_boundary() {
    let mut src = String::from("@Entity\n@Table(name = \"orders\")\npublic class Order {\n    // ");
    // Pad (inside a line comment) so a 2-byte 'é' straddles byte 512 counted from
    // the class-declaration start (byte 0): the old slice `src[0..512]` panics there.
    while src.len() < 511 {
        src.push('x');
    }
    src.push('é');
    src.push_str("\n}\n");

    let unit = JavaProvider::new()
        .parse_file("Order.java", &src)
        .expect("entity should parse");
    let class = unit
        .nodes
        .iter()
        .find(|n| n.kind == cih_core::NodeKind::Class)
        .expect("class node");
    assert_eq!(
        class
            .props
            .as_ref()
            .and_then(|p| p.get("tableName"))
            .and_then(|v| v.as_str()),
        Some("orders"),
    );
}

#[test]
fn field_qualifier_lands_on_type_binding() {
    let src = r#"
        package com.acme;

        class CustomUserImpl implements UserAdmin {
            @Autowired
            @Qualifier("retailUserAdminRef")
            private UserAdmin retailUserAdminRef;

            @Resource(name = "auditLogSvc")
            private AuditLogService auditLogService;

            private UserAdmin unqualified;

            public void run(@Qualifier("other") UserAdmin param) {}
        }
    "#;
    let unit = JavaProvider::new()
        .parse_file("com/acme/CustomUserImpl.java", src)
        .expect("parse");
    let bindings = &unit.parsed_file.type_bindings;
    let by_name = |name: &str| {
        bindings
            .iter()
            .find(|b| b.name == name)
            .unwrap_or_else(|| panic!("binding {name}"))
    };
    assert_eq!(
        by_name("retailUserAdminRef").qualifier.as_deref(),
        Some("retailUserAdminRef")
    );
    assert_eq!(
        by_name("auditLogService").qualifier.as_deref(),
        Some("auditLogSvc")
    );
    assert_eq!(by_name("unqualified").qualifier, None);
    assert_eq!(by_name("param").qualifier.as_deref(), Some("other"));
}

#[test]
fn parameter_qualifier_propagates_only_through_direct_field_assignment() {
    let src = r#"
        package com.acme;

        class Facade {
            private UserAdmin constructorInjected;
            private UserAdmin methodInjected;
            private UserAdmin transformed;

            Facade(@Qualifier("constructorBean") UserAdmin selected) {
                this.constructorInjected = selected;
                this.transformed = decorate(selected);
            }

            void install(@Resource(name = "methodBean") UserAdmin selected) {
                this.methodInjected = selected;
            }
        }
    "#;
    let unit = JavaProvider::new()
        .parse_file("com/acme/Facade.java", src)
        .expect("parse");
    let field = |name: &str| {
        unit.parsed_file
            .type_bindings
            .iter()
            .find(|binding| binding.kind == cih_core::BindingKind::Field && binding.name == name)
            .unwrap_or_else(|| panic!("field binding {name}"))
    };

    assert_eq!(
        field("constructorInjected").qualifier.as_deref(),
        Some("constructorBean")
    );
    assert_eq!(
        field("methodInjected").qualifier.as_deref(),
        Some("methodBean")
    );
    assert_eq!(field("transformed").qualifier, None);
}

#[test]
fn conflicting_direct_and_inferred_field_qualifiers_are_discarded() {
    let src = r#"
        package com.acme;

        class Facade {
            @Qualifier("directBean")
            private UserAdmin service;

            Facade(@Qualifier("constructorBean") UserAdmin selected) {
                this.service = selected;
            }
        }
    "#;
    let unit = JavaProvider::new()
        .parse_file("com/acme/Facade.java", src)
        .expect("parse");
    let field = unit
        .parsed_file
        .type_bindings
        .iter()
        .find(|binding| binding.kind == cih_core::BindingKind::Field && binding.name == "service")
        .expect("field binding");

    assert_eq!(field.qualifier, None);
}

#[test]
fn sql_value_shaped_constants_are_captured_without_upper_snake_names() {
    let src = r#"
        package com.acme;

        class UserDao {
            private static final String insertAuditLog =
                "INSERT INTO AUDIT_LOG (ID, ACTION) VALUES (?, ?)";
            private static final String greetingText = "hello world";
            static final String LEGACY_UPPER = "not sql but captured by name";
        }
    "#;
    let unit = JavaProvider::new()
        .parse_file("com/acme/UserDao.java", src)
        .expect("parse");
    let names: Vec<&str> = unit
        .parsed_file
        .sql_constants
        .iter()
        .map(|c| c.const_name.as_str())
        .collect();
    assert!(names.contains(&"insertAuditLog"), "{names:?}");
    assert!(names.contains(&"LEGACY_UPPER"), "{names:?}");
    assert!(!names.contains(&"greetingText"), "{names:?}");
}

#[test]
fn configured_sql_api_and_const_flow_heuristic_emit_execution_sites() {
    let src = r#"
        package com.acme;

        class AuditAdapter {
            private static final String INSERT_AUDIT =
                "INSERT INTO AUDIT_LOG (ID) VALUES (?)";
            private static final String NOT_SQL = "plain text";
            private AuditQueue auditQueue;

            void record() {
                auditQueue.enqueue(INSERT_AUDIT, 1);
            }

            void trace() {
                logger.info(INSERT_AUDIT);
            }

            void custom() {
                CustomRunner.run(INSERT_AUDIT);
            }

            void inheritedWrapper() {
                enqueue(INSERT_AUDIT);
            }

            void noise() {
                CustomRunner.run(NOT_SQL);
            }
        }
    "#;
    let provider =
        cih_lang::java::JavaProvider::with_sql_apis(vec![cih_lang::java::SqlApi::parse(
            "AuditQueue.enqueue",
        )
        .expect("valid spec")]);
    let unit = provider
        .parse_file("com/acme/AuditAdapter.java", src)
        .expect("parse");
    let sites = &unit.parsed_file.sql_execution_sites;

    let configured = sites
        .iter()
        .find(|s| s.api_name == "AuditQueue.enqueue")
        .expect("configured API site");
    assert_eq!(configured.const_ref.as_deref(), Some("INSERT_AUDIT"));
    assert!(!configured.heuristic, "configured APIs are trusted");

    let heuristic = sites
        .iter()
        .find(|s| s.api_name == "run")
        .expect("heuristic site for SQL constant flowing into a custom call");
    assert_eq!(heuristic.const_ref.as_deref(), Some("INSERT_AUDIT"));
    assert!(heuristic.heuristic);

    let objectless = sites
        .iter()
        .find(|s| s.api_name == "enqueue" && s.heuristic)
        .expect("objectless call should retain the SQL-constant heuristic");
    assert_eq!(objectless.const_ref.as_deref(), Some("INSERT_AUDIT"));

    assert!(
        !sites.iter().any(|s| s.api_name == "info"),
        "logger receivers must not become execution sites"
    );
    assert_eq!(
        sites.len(),
        3,
        "non-SQL constants must not create heuristic sites: {sites:?}"
    );
}

#[test]
fn trivial_accessors_get_the_is_accessor_prop() {
    let src = r#"
        package com.acme;

        class User {
            private String name;
            private boolean active;
            private final String id = "user-id";
            private static final String DEFAULT = "fallback";

            public String getName() { return name; }
            public void setName(String name) { this.name = name; }
            public boolean isActive() { return this.active; }
            public boolean hasName() { return name; }
            public String getId() { return id; }

            // Accessor-shaped names with non-accessor bodies.
            public boolean isConstant() { return true; }
            public String getDefault() { return DEFAULT; }
            public String getDefaultViaThis() { return this.DEFAULT; }
            public String getCalculated() { return name + "!"; }
            public String getConditional() { if (active) return name; return ""; }
            public String getWithArg(String fallback) { return name; }
            public void setCalculated(String name) { this.name = normalize(name); }
            public void setWrong(String value) { this.name = name; }
            public void setMissing() { this.name = "x"; }
            public void setGhost(String ghost) { this.ghost = ghost; }

            // Same prefix, but real logic — must NOT be flagged.
            public String getDisplayName() { return format(name); }
            public void process() { name = "x"; }
        }
    "#;
    let unit = JavaProvider::new()
        .parse_file("com/acme/User.java", src)
        .expect("parse");
    let accessor_flag = |name: &str| {
        unit.nodes
            .iter()
            .find(|n| n.kind == cih_core::NodeKind::Method && n.name == name)
            .unwrap_or_else(|| panic!("method {name}"))
            .props
            .as_ref()
            .and_then(|p| p.get("isAccessor"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    };
    assert!(accessor_flag("getName"));
    assert!(accessor_flag("setName"));
    assert!(accessor_flag("isActive"));
    assert!(accessor_flag("hasName"));
    assert!(accessor_flag("getId"), "instance-final fields are state");
    for name in [
        "isConstant",
        "getDefault",
        "getDefaultViaThis",
        "getCalculated",
        "getConditional",
        "getWithArg",
        "setCalculated",
        "setWrong",
        "setMissing",
        "setGhost",
    ] {
        assert!(!accessor_flag(name), "{name} is not an exact accessor");
    }
    assert!(
        !accessor_flag("getDisplayName"),
        "calls format() — not trivial"
    );
    assert!(!accessor_flag("process"), "no accessor prefix");
}
