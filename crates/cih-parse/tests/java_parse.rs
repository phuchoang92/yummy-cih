use std::fs;
use std::path::{Path, PathBuf};

use cih_core::{constructor_id, field_id, file_id, method_id, type_id, BindingKind, EdgeKind, RefKind};
use cih_parse::{parse_files, LanguageRegistry, ParseOutput};

fn java_registry() -> LanguageRegistry {
    let mut r = LanguageRegistry::new();
    r.register(cih_lang::java::JavaProvider::new());
    r
}

fn temp_repo() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("cih-parse-test-{}-{nanos}", std::process::id()))
}

fn write_file(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn stereotype_of(output: &ParseOutput, fqcn: &str) -> Option<String> {
    output
        .nodes
        .iter()
        .find(|node| node.id == type_id(cih_core::NodeKind::Class, fqcn))
        .and_then(|node| node.props.as_ref())
        .and_then(|props| props.get("stereotype"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn node_prop<'a>(
    output: &'a ParseOutput,
    node_id: &str,
    key: &str,
) -> Option<&'a serde_json::Value> {
    output
        .nodes
        .iter()
        .find(|n| n.id.as_str() == node_id)
        .and_then(|n| n.props.as_ref())
        .and_then(|p| p.get(key))
}

#[test]
fn parses_java_structure_ir_and_references() {
    let root = temp_repo();
    let rel = "src/main/java/com/example/OwnerController.java";
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        r#"
package com.example;

import java.util.List;
import static com.example.Util.*;

class Base {}
interface Handler {}

@RestController
@RequestMapping(path = "/owners")
class OwnerController extends Base implements Handler {
private OwnerService service;

public OwnerController(OwnerService service) {
    this.service = service;
}

@GetMapping("/{id}")
public Owner findOwner(Long id) {
    return service.findOwner(id);
}

@PostMapping(path = "/search", produces = "application/json")
public void search() {
    service.findOwner(1L);
}

class Inner {
    void ping() {
        helper();
    }
}
}
"#,
    )
    .unwrap();

    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    fs::remove_dir_all(&root).unwrap();

    let parsed = output.parsed_files.first().unwrap();
    assert_eq!(parsed.package.as_deref(), Some("com.example"));
    assert!(parsed.imports.iter().any(|imp| imp.raw == "java.util.List"));
    assert!(parsed
        .imports
        .iter()
        .any(|imp| imp.raw == "com.example.Util.*" && imp.is_static && imp.is_wildcard));

    assert!(parsed.defs.iter().any(|def| {
        def.kind == cih_core::NodeKind::Class && def.fqcn == "com.example.OwnerController"
    }));
    assert!(parsed.defs.iter().any(|def| {
        def.kind == cih_core::NodeKind::Class
            && def.fqcn == "com.example.OwnerController.Inner"
            && def.owner
                == Some(type_id(
                    cih_core::NodeKind::Class,
                    "com.example.OwnerController",
                ))
    }));
    assert!(parsed.defs.iter().any(|def| {
        def.kind == cih_core::NodeKind::Method
            && def.name == "findOwner"
            && def.id == method_id("com.example.OwnerController", "findOwner", 1)
    }));
    assert!(parsed.defs.iter().any(|def| {
        def.kind == cih_core::NodeKind::Constructor
            && def.id == constructor_id("com.example.OwnerController", 1)
    }));
    assert!(parsed.defs.iter().any(|def| {
        def.kind == cih_core::NodeKind::Field
            && def.id == field_id("com.example.OwnerController", "service")
    }));

    assert!(parsed.reference_sites.iter().any(|site| {
        site.kind == RefKind::Call
            && site.name == "findOwner"
            && site.receiver.as_deref() == Some("service")
            && site.arity == Some(1)
            && site.in_fqcn == "com.example.OwnerController#findOwner/1"
    }));
    assert!(parsed
        .reference_sites
        .iter()
        .any(|site| site.kind == RefKind::Extends && site.name == "Base"));
    assert!(parsed
        .reference_sites
        .iter()
        .any(|site| site.kind == RefKind::Implements && site.name == "Handler"));

    assert!(output
        .nodes
        .iter()
        .any(|node| node.id == file_id(rel) && node.kind == cih_core::NodeKind::File));
    let controller = output
        .nodes
        .iter()
        .find(|node| {
            node.id == type_id(cih_core::NodeKind::Class, "com.example.OwnerController")
        })
        .unwrap();
    assert_eq!(
        controller
            .props
            .as_ref()
            .and_then(|props| props.get("stereotype"))
            .and_then(|value| value.as_str()),
        Some("controller")
    );
    assert!(output.edges.iter().any(|edge| {
        edge.kind == EdgeKind::HasMethod
            && edge.src == type_id(cih_core::NodeKind::Class, "com.example.OwnerController")
            && edge.dst == method_id("com.example.OwnerController", "findOwner", 1)
    }));
    assert!(output.edges.iter().any(|edge| {
        edge.kind == EdgeKind::Contains
            && edge.src == type_id(cih_core::NodeKind::Class, "com.example.OwnerController")
            && edge.dst
                == type_id(
                    cih_core::NodeKind::Class,
                    "com.example.OwnerController.Inner",
                )
    }));
    let route_id = cih_core::NodeId::new("Route:GET /owners/{id}");
    assert!(output.nodes.iter().any(|node| {
        node.id == route_id
            && node.kind == cih_core::NodeKind::Route
            && node
                .props
                .as_ref()
                .and_then(|props| props.get("httpMethod"))
                .and_then(|value| value.as_str())
                == Some("GET")
    }));
    assert!(output.edges.iter().any(|edge| {
        edge.kind == EdgeKind::HandlesRoute
            && edge.src == method_id("com.example.OwnerController", "findOwner", 1)
            && edge.dst == route_id
    }));
    assert!(!output
        .nodes
        .iter()
        .any(|node| node.id.as_str() == "Route:POST /owners/application/json"));
}

#[test]
fn unreadable_file_is_skipped_without_aborting() {
    let root = temp_repo();
    let good = "src/main/java/com/example/Ok.java";
    let good_path = root.join(good);
    fs::create_dir_all(good_path.parent().unwrap()).unwrap();
    fs::write(&good_path, "package com.example;\nclass Ok {}\n").unwrap();

    let missing = "src/main/java/com/example/Missing.java";
    let output = parse_files(
        &root,
        &[good.to_string(), missing.to_string()],
        &java_registry(),
    )
    .unwrap();
    fs::remove_dir_all(&root).unwrap();

    assert_eq!(output.parsed_files.len(), 1);
    assert_eq!(output.parsed_files[0].file, good);
    assert_eq!(output.skipped.len(), 1);
    assert_eq!(output.skipped[0].rel, missing);
    assert!(output
        .nodes
        .iter()
        .any(|node| node.id == type_id(cih_core::NodeKind::Class, "com.example.Ok")));
}

#[test]
fn explicit_receiver_parameter_excluded_from_arity() {
    let root = temp_repo();
    let rel = "src/main/java/com/example/Receiver.java";
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        "package com.example;\nclass Receiver {\n  void touch(Receiver this, int x) {}\n}\n",
    )
    .unwrap();

    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    fs::remove_dir_all(&root).unwrap();

    let parsed = output.parsed_files.first().unwrap();
    assert!(parsed.defs.iter().any(|def| {
        def.kind == cih_core::NodeKind::Method
            && def.id == method_id("com.example.Receiver", "touch", 1)
    }));
}

#[test]
fn stereotype_uses_own_annotations_not_body() {
    let root = temp_repo();
    let rel = "src/main/java/com/example/Stereotypes.java";
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        r#"
package com.example;
// A @Service whose body has a @GetMapping method must NOT be tagged a controller.
@Service
class FooService {
@GetMapping("/x")
public void m() {}
}
@Repository
class FooRepo {}
@Entity
class FooEntity {}
class Plain {}
"#,
    )
    .unwrap();

    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    fs::remove_dir_all(&root).unwrap();

    assert_eq!(
        stereotype_of(&output, "com.example.FooService").as_deref(),
        Some("service"),
        "a @Service with a @GetMapping body must stay a service"
    );
    assert_eq!(
        stereotype_of(&output, "com.example.FooRepo").as_deref(),
        Some("repository")
    );
    assert_eq!(
        stereotype_of(&output, "com.example.FooEntity").as_deref(),
        Some("entity")
    );
    assert_eq!(stereotype_of(&output, "com.example.Plain"), None);
}

#[test]
fn persists_type_bindings_param_return_field_and_in_callable() {
    let root = temp_repo();
    let rel = "src/main/java/com/example/OwnerController.java";
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        r#"
package com.example;
class OwnerController {
private OwnerService service;
public Owner findOwner(Long id) {
    var found = service.findOwner(id);
    return found;
}
}
"#,
    )
    .unwrap();

    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    fs::remove_dir_all(&root).unwrap();
    let parsed = output.parsed_files.first().unwrap();

    let method = parsed
        .defs
        .iter()
        .find(|d| d.id == method_id("com.example.OwnerController", "findOwner", 1))
        .unwrap();
    assert_eq!(method.param_types, vec!["Long"]);
    assert_eq!(method.return_type.as_deref(), Some("Owner"));
    let field = parsed
        .defs
        .iter()
        .find(|d| d.id == field_id("com.example.OwnerController", "service"))
        .unwrap();
    assert_eq!(field.declared_type.as_deref(), Some("OwnerService"));

    let binding = |name: &str| {
        parsed
            .type_bindings
            .iter()
            .find(|b| b.name == name)
            .cloned()
            .unwrap_or_else(|| panic!("no binding for {name}"))
    };
    let svc = binding("service");
    assert_eq!(svc.kind, BindingKind::Field);
    assert_eq!(svc.raw_type, "OwnerService");
    assert_eq!(svc.in_fqcn, "com.example.OwnerController");

    let id = binding("id");
    assert_eq!(id.kind, BindingKind::Param);
    assert_eq!(id.raw_type, "Long");
    assert_eq!(id.in_fqcn, "com.example.OwnerController#findOwner/1");

    let found = binding("found");
    assert_eq!(found.kind, BindingKind::CallResult);
    assert_eq!(found.raw_type, "findOwner");

    let call = parsed
        .reference_sites
        .iter()
        .find(|s| s.kind == RefKind::Call && s.name == "findOwner")
        .unwrap();
    assert_eq!(
        call.in_callable,
        method_id("com.example.OwnerController", "findOwner", 1)
    );
    assert_eq!(call.in_fqcn, "com.example.OwnerController#findOwner/1");
}

#[test]
fn array_form_mapping_yields_all_routes() {
    let root = temp_repo();
    let rel = "src/main/java/com/example/Multi.java";
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        r#"
package com.example;
@RestController
class Multi {
@GetMapping({"/a", "/b"})
public void m() {}
}
"#,
    )
    .unwrap();

    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    fs::remove_dir_all(&root).unwrap();

    for path in ["Route:GET /a", "Route:GET /b"] {
        assert!(
            output.nodes.iter().any(|node| node.id.as_str() == path),
            "expected route {path}"
        );
    }
}

#[test]
fn bean_method_tagged_when_annotated() {
    let root = temp_repo();
    let rel = "src/main/java/com/example/AppConfig.java";
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        r#"
package com.example;
@Configuration
class AppConfig {
@Bean
public DataSource dataSource() { return null; }
public void helper() {}
}
"#,
    )
    .unwrap();

    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    fs::remove_dir_all(&root).unwrap();

    let bean_id = method_id("com.example.AppConfig", "dataSource", 0);
    let helper_id = method_id("com.example.AppConfig", "helper", 0);
    assert_eq!(
        node_prop(&output, bean_id.as_str(), "isBean"),
        Some(&serde_json::Value::Bool(true)),
        "@Bean method must have isBean=true"
    );
    assert_eq!(
        node_prop(&output, helper_id.as_str(), "isBean"),
        None,
        "plain method must have no isBean prop"
    );
}

#[test]
fn bean_method_not_tagged_without_annotation() {
    let root = temp_repo();
    let rel = "src/main/java/com/example/Plain.java";
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        "package com.example;\nclass Plain { public Object produce() { return null; } }\n",
    )
    .unwrap();

    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    fs::remove_dir_all(&root).unwrap();

    let id = method_id("com.example.Plain", "produce", 0);
    assert_eq!(node_prop(&output, id.as_str(), "isBean"), None);
}

#[test]
fn jpa_repository_tagged_as_repository() {
    let root = temp_repo();
    let rel = "src/main/java/com/example/UserRepo.java";
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        "package com.example;\nclass UserRepo implements JpaRepository<User, Long> {}\n",
    )
    .unwrap();

    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    fs::remove_dir_all(&root).unwrap();

    assert_eq!(
        stereotype_of(&output, "com.example.UserRepo").as_deref(),
        Some("repository"),
        "JpaRepository implementor must be tagged as repository"
    );
    assert_eq!(
        node_prop(&output, "Class:com.example.UserRepo", "entityType"),
        Some(&serde_json::Value::String("User".to_string())),
        "entityType must be the first generic type argument"
    );
}

#[test]
fn jpa_crud_repository_also_tagged() {
    let root = temp_repo();
    let rel = "src/main/java/com/example/ItemRepo.java";
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        "package com.example;\nclass ItemRepo implements CrudRepository<Item, Long> {}\n",
    )
    .unwrap();

    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    fs::remove_dir_all(&root).unwrap();

    assert_eq!(
        stereotype_of(&output, "com.example.ItemRepo").as_deref(),
        Some("repository")
    );
}

#[test]
fn jpa_annotation_idempotent_with_interface() {
    let root = temp_repo();
    let rel = "src/main/java/com/example/AnnotatedRepo.java";
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        "package com.example;\n@Repository\nclass AnnotatedRepo implements JpaRepository<Order, Long> {}\n",
    )
    .unwrap();

    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    fs::remove_dir_all(&root).unwrap();

    assert_eq!(
        stereotype_of(&output, "com.example.AnnotatedRepo").as_deref(),
        Some("repository"),
        "stereotype must be repository when both annotation and interface are present"
    );
}

#[test]
fn test_class_by_annotation_gets_test_stereotype() {
    let root = temp_repo();
    let rel = "src/test/java/com/example/OrderServiceTest.java";
    write_file(
        &root,
        rel,
        r#"
package com.example;
import org.springframework.boot.test.context.SpringBootTest;
@SpringBootTest
public class OrderServiceTest {
@Test public void testSave() {}
}
"#,
    );
    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    fs::remove_dir_all(&root).unwrap();

    let test_node = output
        .nodes
        .iter()
        .find(|n| n.name == "OrderServiceTest")
        .expect("OrderServiceTest node must exist");
    let stereotype = test_node
        .props
        .as_ref()
        .and_then(|p| p.get("stereotype"))
        .and_then(|v| v.as_str());
    assert_eq!(
        stereotype,
        Some("test"),
        "SpringBootTest class must have stereotype=test"
    );
}

#[test]
fn test_class_by_naming_convention_gets_test_stereotype() {
    let root = temp_repo();
    let rel = "src/test/java/com/example/PaymentServiceIT.java";
    write_file(
        &root,
        rel,
        "package com.example;\npublic class PaymentServiceIT {}\n",
    );
    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    fs::remove_dir_all(&root).unwrap();

    let test_node = output
        .nodes
        .iter()
        .find(|n| n.name == "PaymentServiceIT")
        .expect("PaymentServiceIT node must exist");
    let stereotype = test_node
        .props
        .as_ref()
        .and_then(|p| p.get("stereotype"))
        .and_then(|v| v.as_str());
    assert_eq!(
        stereotype,
        Some("test"),
        "*IT class must have stereotype=test"
    );
}

#[test]
fn name_suffix_fallback_stereotypes() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "CartEndpoint",
            "package com.example;\npublic class CartEndpoint {}\n",
            "controller",
        ),
        (
            "OrderResource",
            "package com.example;\npublic class OrderResource {}\n",
            "resource",
        ),
        (
            "PaymentApi",
            "package com.example;\npublic class PaymentApi {}\n",
            "controller",
        ),
        (
            "CheckoutHandler",
            "package com.example;\npublic class CheckoutHandler {}\n",
            "handler",
        ),
        (
            "PricingFacade",
            "package com.example;\npublic class PricingFacade {}\n",
            "service",
        ),
        (
            "ItemRepository",
            "package com.example;\npublic class ItemRepository {}\n",
            "repository",
        ),
        (
            "InventoryService",
            "package com.example;\npublic class InventoryService {}\n",
            "service",
        ),
    ];
    for (class_name, src, expected) in cases {
        let root = temp_repo();
        let rel = format!("src/main/java/com/example/{class_name}.java");
        write_file(&root, &rel, src);
        let output = parse_files(&root, std::slice::from_ref(&rel), &java_registry()).unwrap();
        fs::remove_dir_all(&root).unwrap();
        let node = output
            .nodes
            .iter()
            .find(|n| &n.name == class_name)
            .unwrap_or_else(|| panic!("{class_name} node must exist"));
        let stereotype = node
            .props
            .as_ref()
            .and_then(|p| p.get("stereotype"))
            .and_then(|v| v.as_str());
        assert_eq!(
            stereotype,
            Some(*expected),
            "{class_name} must have stereotype={expected}, got {stereotype:?}"
        );
    }
}

#[test]
fn annotation_stereotype_wins_over_name_suffix() {
    let root = temp_repo();
    let rel = "src/main/java/com/example/CartController.java";
    write_file(&root, rel,
        "package com.example;\nimport org.springframework.framework_role.Service;\n@Service\npublic class CartController {}\n",
    );
    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    fs::remove_dir_all(&root).unwrap();
    let node = output
        .nodes
        .iter()
        .find(|n| n.name == "CartController")
        .expect("node must exist");
    let stereotype = node
        .props
        .as_ref()
        .and_then(|p| p.get("stereotype"))
        .and_then(|v| v.as_str());
    assert_eq!(
        stereotype,
        Some("service"),
        "@Service must win over Controller suffix"
    );
}

#[test]
fn registry_dispatches_to_correct_provider_by_extension() {
    let mut r = LanguageRegistry::new();
    r.register(cih_lang::java::JavaProvider::new());

    assert!(
        r.provider_for("Foo.java").is_some(),
        "Java provider should match .java"
    );
    assert!(
        r.provider_for("Foo.py").is_none(),
        "No provider registered for .py"
    );
    assert!(
        r.provider_for("Foo.txt").is_none(),
        "No provider registered for .txt"
    );

    let exts = r.all_extensions();
    assert!(
        exts.contains(&".java"),
        "all_extensions should include .java"
    );
    assert!(!exts.contains(&".py"), ".py not registered");
}

#[test]
fn bare_mapping_real_cart_controller() {
    let repo = std::path::Path::new("/Users/phuc/BigMoves/dienmaychiben/212ecom-be");
    let rel = "src/main/java/org/phuc/commerce/modules/order/controller/CartController.java";
    if !repo.join(rel).exists() {
        return;
    }
    let output = parse_files(repo, &[rel.to_string()], &java_registry()).unwrap();
    let route_ids: Vec<String> = output
        .nodes
        .iter()
        .filter(|n| n.id.as_str().starts_with("Route:"))
        .map(|n| n.id.as_str().to_string())
        .collect();
    assert!(
        route_ids.iter().any(|id| id == "Route:GET /api/v1/cart"),
        "bare @GetMapping must produce Route:GET /api/v1/cart, got: {:?}",
        route_ids
    );
    assert!(
        route_ids.iter().any(|id| id == "Route:DELETE /api/v1/cart"),
        "bare @DeleteMapping must produce Route:DELETE /api/v1/cart, got: {:?}",
        route_ids
    );
}

#[test]
fn bare_get_mapping_emits_route_at_class_prefix() {
    let root = temp_repo();
    let rel = "src/main/java/com/example/CartController.java";
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, r#"
package com.example;
@RestController
@RequestMapping("/api/v1/cart")
class CartController {
@GetMapping
public Object getMyCart(@AuthenticationPrincipal UserDetails userDetails) { return null; }

@DeleteMapping
public Object clearMyCart(@AuthenticationPrincipal UserDetails userDetails) { return null; }

@PostMapping("/items")
public Object addItem(@AuthenticationPrincipal UserDetails userDetails, Object req) { return null; }
}
"#).unwrap();

    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    fs::remove_dir_all(&root).unwrap();

    let route_ids: Vec<String> = output
        .nodes
        .iter()
        .filter(|n| n.id.as_str().starts_with("Route:"))
        .map(|n| n.id.as_str().to_string())
        .collect();

    assert!(
        route_ids.iter().any(|id| id == "Route:GET /api/v1/cart"),
        "bare @GetMapping must produce Route:GET /api/v1/cart, got: {:?}",
        route_ids
    );
    assert!(
        route_ids.iter().any(|id| id == "Route:DELETE /api/v1/cart"),
        "bare @DeleteMapping must produce Route:DELETE /api/v1/cart, got: {:?}",
        route_ids
    );
    assert!(
        route_ids
            .iter()
            .any(|id| id == "Route:POST /api/v1/cart/items"),
        "got: {:?}",
        route_ids
    );
}
