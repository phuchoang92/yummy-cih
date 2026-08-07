use cih_core::{ContractKind, ContractSite, MessagingFramework, NodeKind};
use cih_lang::{kotlin::KotlinProvider, LanguageProvider};

#[test]
fn test_kotlin_parse_basic() {
    let provider = KotlinProvider::new();
    let src = r#"package com.example.service

import com.example.model.User
import com.example.repo.*

class UserService(
    private val userRepo: String
) {
    fun findUser(id: Long): String {
        return ""
    }
}

interface UserRepository {
    fun findById(id: Long): String
}

object UserCache {
    fun get(id: Long): String = ""
}
"#;
    let unit = provider
        .parse_file("src/main/kotlin/UserService.kt", src)
        .unwrap();
    assert_eq!(
        unit.parsed_file.package.as_deref(),
        Some("com.example.service")
    );
    assert_eq!(unit.parsed_file.imports.len(), 2);
    assert!(!unit.parsed_file.imports[0].is_wildcard);
    assert!(unit.parsed_file.imports[1].is_wildcard);
    let class_nodes: Vec<_> = unit
        .nodes
        .iter()
        .filter(|n| {
            matches!(
                n.kind,
                cih_core::NodeKind::Class | cih_core::NodeKind::Interface
            )
        })
        .collect();
    assert!(
        class_nodes.len() >= 3,
        "expected >=3 type nodes, got {}",
        class_nodes.len()
    );
    let method_nodes: Vec<_> = unit
        .nodes
        .iter()
        .filter(|n| {
            matches!(
                n.kind,
                cih_core::NodeKind::Method | cih_core::NodeKind::Function
            )
        })
        .collect();
    assert!(
        method_nodes.len() >= 2,
        "expected >=2 method nodes, got {}",
        method_nodes.len()
    );
}

// ── Routes + contract sites (Phase A: Spring/Feign/Kafka detection) ─────────

fn parse(src: &str) -> cih_core::ParsedUnit {
    KotlinProvider::new()
        .parse_file("src/main/kotlin/Sample.kt", src)
        .expect("sample should parse")
}

fn contract_sites(src: &str) -> Vec<ContractSite> {
    parse(src).parsed_file.contract_sites
}

fn route_nodes(src: &str) -> Vec<cih_core::Node> {
    parse(src)
        .nodes
        .into_iter()
        .filter(|n| n.kind == NodeKind::Route)
        .collect()
}

#[test]
fn spring_route_with_class_prefix() {
    let src = r#"package com.acme

@RestController
@RequestMapping("/api/orders")
class OrderController {
    @GetMapping("/{id}")
    fun get(id: Long): String = ""
}
"#;
    let unit = parse(src);
    let routes: Vec<_> = unit
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Route)
        .collect();
    assert_eq!(routes.len(), 1, "expected one route, got {routes:?}");
    let route = routes[0];
    assert_eq!(route.id.as_str(), "Route:GET /api/orders/{id}");
    let props = route.props.as_ref().expect("route props");
    assert_eq!(props["httpMethod"], "GET");
    assert_eq!(props["path"], "/api/orders/{id}");
    assert_eq!(props["source"], "spring_mvc");
    assert_eq!(props["handler"], "com.acme.OrderController#get/1");

    let handles: Vec<_> = unit
        .edges
        .iter()
        .filter(|e| e.kind == cih_core::EdgeKind::HandlesRoute)
        .collect();
    assert_eq!(handles.len(), 1);
    assert_eq!(
        handles[0].src.as_str(),
        "Method:com.acme.OrderController#get/1"
    );
    assert_eq!(handles[0].dst.as_str(), "Route:GET /api/orders/{id}");
}

#[test]
fn rest_template_call_is_http_call() {
    let src = r#"package com.acme

class OrderClient(private val restTemplate: RestTemplate) {
    fun fetch(id: Long): String {
        return restTemplate.getForObject("/api/orders/1", String::class.java)
    }
}
"#;
    let sites = contract_sites(src);
    assert_eq!(sites.len(), 1, "expected one site, got {sites:?}");
    let site = &sites[0];
    assert_eq!(site.kind, ContractKind::HttpCall);
    assert_eq!(site.http_method.as_deref(), Some("GET"));
    assert_eq!(site.url_template.as_deref(), Some("/api/orders/1"));
    assert_eq!(
        site.in_callable.as_str(),
        "Method:com.acme.OrderClient#fetch/1"
    );
}

#[test]
fn untyped_receiver_emits_nothing() {
    let src = r#"package com.acme

class NotAClient {
    fun fetch(): String {
        return somethingElse.getForObject("/api/orders/1", String::class.java)
    }
}
"#;
    assert!(contract_sites(src).is_empty());
}

#[test]
fn webclient_chain_is_http_call() {
    let src = r#"package com.acme

class PriceClient {
    private val webClient: WebClient = WebClient.create()

    fun fetch(): String {
        return webClient.get().uri("/api/prices").retrieve().toString()
    }
}
"#;
    let sites = contract_sites(src);
    let site = sites
        .iter()
        .find(|s| s.kind == ContractKind::HttpCall)
        .expect("webclient http call site");
    assert_eq!(site.http_method.as_deref(), Some("GET"));
    assert_eq!(site.url_template.as_deref(), Some("/api/prices"));
}

#[test]
fn interpolated_url_yields_site_without_template() {
    let src = r#"package com.acme

class OrderClient(private val restTemplate: RestTemplate) {
    fun fetch(id: Long): String {
        return restTemplate.getForObject("/api/orders/$id", String::class.java)
    }
}
"#;
    let sites = contract_sites(src);
    assert_eq!(sites.len(), 1);
    // Phase A is literal-only: the call is detected, the dynamic URL is not.
    assert_eq!(sites[0].url_template, None);
}

#[test]
fn feign_interface_is_http_client_proxy() {
    let src = r#"package com.acme

@FeignClient(name = "billing", url = "/billing")
interface BillingClient {
    @GetMapping("/invoices/{id}")
    fun invoice(id: Long): String
}
"#;
    let sites = contract_sites(src);
    let site = sites
        .iter()
        .find(|s| s.kind == ContractKind::HttpClientProxy)
        .expect("feign proxy site");
    assert_eq!(site.http_method.as_deref(), Some("GET"));
    assert_eq!(site.url_template.as_deref(), Some("/billing/invoices/{id}"));
    assert_eq!(
        site.in_callable.as_str(),
        "Method:com.acme.BillingClient#invoice/1"
    );
}

#[test]
fn kafka_listener_is_event_listen_kafka() {
    let src = r#"package com.acme

class OrderConsumer {
    @KafkaListener(topics = ["orders.created"])
    fun onOrder(msg: String) {}
}
"#;
    let sites = contract_sites(src);
    let site = sites
        .iter()
        .find(|s| s.topic.as_deref() == Some("orders.created"))
        .expect("kafka listener contract site");
    assert_eq!(site.kind, ContractKind::EventListen);
    assert_eq!(site.messaging_framework, Some(MessagingFramework::Kafka));
}

#[test]
fn spring_event_listener_is_event_listen_spring() {
    let src = r#"package com.acme

class UserListener {
    @EventListener
    fun on(event: UserSaved) {}
}
"#;
    let sites = contract_sites(src);
    let site = sites
        .iter()
        .find(|s| s.kind == ContractKind::EventListen)
        .expect("spring @EventListener contract site");
    assert_eq!(site.topic.as_deref(), Some("UserSaved"));
    assert_eq!(site.messaging_framework, Some(MessagingFramework::Spring));
}

#[test]
fn kafka_template_send_is_event_publish_kafka() {
    let src = r#"package com.acme

class OrderProducer(private val kafkaTemplate: KafkaTemplate<String, String>) {
    fun place() {
        kafkaTemplate.send("orders.created", "payload")
    }
}
"#;
    let sites = contract_sites(src);
    let site = sites
        .iter()
        .find(|s| s.kind == ContractKind::EventPublish)
        .expect("kafka publish contract site");
    assert_eq!(site.topic.as_deref(), Some("orders.created"));
    assert_eq!(site.messaging_framework, Some(MessagingFramework::Kafka));
}

#[test]
fn publish_event_is_event_publish_spring() {
    let src = r#"package com.acme

class OrderService(private val publisher: ApplicationEventPublisher) {
    fun place(id: Long) {
        publisher.publishEvent(OrderPlaced(id))
    }
}
"#;
    let sites = contract_sites(src);
    let site = sites
        .iter()
        .find(|s| s.kind == ContractKind::EventPublish)
        .expect("spring publish contract site");
    assert_eq!(site.topic.as_deref(), Some("OrderPlaced"));
    assert_eq!(site.messaging_framework, Some(MessagingFramework::Spring));
}

#[test]
fn route_annotation_without_path_uses_prefix_only() {
    let src = r#"package com.acme

@RestController
@RequestMapping("/health")
class HealthController {
    @GetMapping
    fun check(): String = "ok"
}
"#;
    let routes = route_nodes(src);
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].id.as_str(), "Route:GET /health");
}

// ── Dynamic-URL parts (Phase B: interpolation + concat → url_parts) ─────────

#[test]
fn interpolated_url_yields_parts() {
    use cih_core::UrlPart;
    let src = r#"package com.acme

class OrderClient(private val restTemplate: RestTemplate) {
    fun fetch(id: Long): String {
        return restTemplate.getForObject("$BASE/items/$id", String::class.java)
    }
}
"#;
    let sites = contract_sites(src);
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].url_template, None);
    assert_eq!(
        sites[0].url_parts.as_deref(),
        Some(
            &[
                UrlPart::ConstRef("BASE".into()),
                UrlPart::Lit("/items/".into()),
                UrlPart::ConstRef("id".into()),
            ][..]
        )
    );
}

#[test]
fn interpolated_expression_is_dynamic_part() {
    use cih_core::UrlPart;
    let src = r#"package com.acme

class OrderClient(private val restTemplate: RestTemplate) {
    fun fetch(): String {
        return restTemplate.getForObject("${svc.base()}/x", String::class.java)
    }
}
"#;
    let sites = contract_sites(src);
    let parts = sites[0].url_parts.as_deref().expect("parts");
    assert!(parts.contains(&UrlPart::Dynamic));
    assert!(parts.contains(&UrlPart::Lit("/x".into())));
}

#[test]
fn plus_concat_yields_parts() {
    use cih_core::UrlPart;
    let src = r#"package com.acme

class OrderClient(private val restTemplate: RestTemplate) {
    fun fetch(): String {
        return restTemplate.getForObject(Constants.BASE + "/items", String::class.java)
    }
}
"#;
    let sites = contract_sites(src);
    assert_eq!(
        sites[0].url_parts.as_deref(),
        Some(
            &[
                UrlPart::ConstRef("Constants.BASE".into()),
                UrlPart::Lit("/items".into()),
            ][..]
        )
    );
}

#[test]
fn companion_object_constants_are_indexed_on_outer_class() {
    let src = r#"package com.acme

class OrderClient {
    companion object {
        const val BASE = "/api/orders"
    }
}

object Endpoints {
    val ITEMS = "/api/items"
}
"#;
    let constants = parse(src).parsed_file.string_constants;
    let base = constants
        .iter()
        .find(|c| c.const_name == "BASE")
        .expect("companion constant");
    assert_eq!(base.owner_fqcn, "com.acme.OrderClient");
    assert_eq!(base.value, "/api/orders");
    let items = constants
        .iter()
        .find(|c| c.const_name == "ITEMS")
        .expect("object constant");
    assert_eq!(items.owner_fqcn, "com.acme.Endpoints");
    assert_eq!(items.value, "/api/items");
}
