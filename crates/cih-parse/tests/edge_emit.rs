use std::fs;
use std::path::{Path, PathBuf};

use cih_core::{method_id, type_id, ContractKind, EdgeKind};
use cih_parse::{parse_files, LanguageRegistry};

fn java_registry() -> LanguageRegistry {
    let mut r = LanguageRegistry::new();
    r.register(cih_lang::java::JavaProvider::new());
    r
}

fn temp_repo() -> PathBuf {
    // pid + atomic counter: parallel tests in one binary share a pid and can
    // race to the same SystemTime nanos, so a timestamp alone collides.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "cih-parse-test-{}-{seq}-{nanos}",
        std::process::id()
    ))
}

fn write_file(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn parses_cross_service_contract_sites() {
    let root = temp_repo();
    let rel = "src/main/java/com/example/Contracts.java";
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        r#"
package com.example;

@FeignClient(name = "orders", path = "/orders")
interface OrdersClient {
@GetMapping("/{id}")
Order getOrder(String id);
}

class ContractClient {
private RestTemplate restTemplate;
private WebClient webClient;
private KafkaTemplate<String, String> kafkaTemplate;
private ApplicationEventPublisher publisher;

@KafkaListener(topics = {"orders.created"})
void listen(String payload) {}

@EventListener
void onUserCreated(UserCreated event) {}

void call() {
    restTemplate.getForObject("http://orders.local/api/orders/{id}", String.class);
    webClient.post().uri("/api/payments").retrieve();
    kafkaTemplate.send("orders.created", "1");
    publisher.publishEvent(new UserCreated());
}
}
"#,
    )
    .unwrap();

    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    fs::remove_dir_all(&root).unwrap();
    let parsed = output.parsed_files.first().unwrap();

    assert!(parsed.contract_sites.iter().any(|site| {
        site.kind == ContractKind::HttpClientProxy
            && site.http_method.as_deref() == Some("GET")
            && site.url_template.as_deref() == Some("/orders/{id}")
            && site.in_callable == method_id("com.example.OrdersClient", "getOrder", 1)
    }));
    assert!(parsed.contract_sites.iter().any(|site| {
        site.kind == ContractKind::HttpCall
            && site.http_method.as_deref() == Some("GET")
            && site.url_template.as_deref() == Some("/api/orders/{id}")
            && site.in_callable == method_id("com.example.ContractClient", "call", 0)
    }));
    assert!(parsed.contract_sites.iter().any(|site| {
        site.kind == ContractKind::HttpCall
            && site.http_method.as_deref() == Some("POST")
            && site.url_template.as_deref() == Some("/api/payments")
    }));
    assert!(parsed.contract_sites.iter().any(|site| {
        site.kind == ContractKind::EventListen
            && site.topic.as_deref() == Some("orders.created")
            && site.in_callable == method_id("com.example.ContractClient", "listen", 1)
    }));
    assert!(parsed.contract_sites.iter().any(|site| {
        site.kind == ContractKind::EventListen && site.topic.as_deref() == Some("UserCreated")
    }));
    assert!(parsed.contract_sites.iter().any(|site| {
        site.kind == ContractKind::EventPublish
            && site.topic.as_deref() == Some("orders.created")
            && site.in_callable == method_id("com.example.ContractClient", "call", 0)
    }));
    assert!(parsed.contract_sites.iter().any(|site| {
        site.kind == ContractKind::EventPublish && site.topic.as_deref() == Some("UserCreated")
    }));
}

#[test]
fn test_method_emits_tests_edge_and_prop() {
    let root = temp_repo();
    let rel = "src/test/java/com/example/FooTest.java";
    write_file(
        &root,
        rel,
        r#"
package com.example;
public class FooTest {
@Test
public void shouldWork() {}
public void helperMethod() {}
}
"#,
    );
    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    fs::remove_dir_all(&root).unwrap();

    let test_method = output
        .nodes
        .iter()
        .find(|n| n.name == "shouldWork")
        .expect("shouldWork method node must exist");
    let is_test = test_method
        .props
        .as_ref()
        .and_then(|p| p.get("isTest"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    assert!(is_test, "@Test method must have isTest=true prop");

    let test_class_id = type_id(cih_core::NodeKind::Class, "com.example.FooTest");
    let test_method_id = method_id("com.example.FooTest", "shouldWork", 0);
    assert!(
        output.edges.iter().any(|e| {
            e.kind == EdgeKind::Tests && e.src == test_method_id && e.dst == test_class_id
        }),
        "TESTS edge from @Test method to owner class must be emitted"
    );

    let helper_id = method_id("com.example.FooTest", "helperMethod", 0);
    assert!(
        !output
            .edges
            .iter()
            .any(|e| e.kind == EdgeKind::Tests && e.src == helper_id),
        "non-@Test method must not emit a TESTS edge"
    );
}

#[test]
fn mock_bean_field_emits_tests_edge() {
    let root = temp_repo();
    let rel = "src/test/java/com/example/BarTest.java";
    write_file(
        &root,
        rel,
        r#"
package com.example;
@SpringBootTest
public class BarTest {
@MockBean
private OrderService orderService;
}
"#,
    );
    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    fs::remove_dir_all(&root).unwrap();

    let test_class_id = type_id(cih_core::NodeKind::Class, "com.example.BarTest");
    assert!(
        output.edges.iter().any(|e| {
            e.kind == EdgeKind::Tests
                && e.src == test_class_id
                && e.dst.as_str() == "Class:OrderService"
        }),
        "TESTS edge from test class to @MockBean field type must be emitted"
    );
}
