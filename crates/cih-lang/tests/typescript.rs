use cih_core::NodeKind;
use cih_lang::{typescript::TypescriptProvider, LanguageProvider};

const NESTJS_SAMPLE: &str = r#"
import { Controller, Get, Post } from '@nestjs/common';

@Controller('orders')
export class OrderController {
@Get(':id')
findOne(id: string) {
    return null;
}

@Post()
create(body: any) {
    return null;
}
}
"#;

#[test]
fn nestjs_routes_extracted_with_correct_path() {
    let provider = TypescriptProvider::new();
    let unit = provider
        .parse_file("src/orders/order.controller.ts", NESTJS_SAMPLE)
        .expect("should parse");
    let routes: Vec<_> = unit
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Route)
        .collect();
    let names: Vec<&str> = routes.iter().map(|n| n.name.as_str()).collect();
    assert!(
        names.iter().any(|n| n.contains("GET")),
        "expected a GET route, got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("POST")),
        "expected a POST route, got: {names:?}"
    );
    for route in &routes {
        let props = route.props.as_ref().expect("route has props");
        assert!(
            props["source"].as_str().unwrap_or("").contains("nest"),
            "source should be nest_js, got: {props}"
        );
    }
}

const PLAIN_SAMPLE: &str = r#"
export class UserService {
findAll() { return []; }
findOne(id: string) { return null; }
}

export function greet(name: string): string {
return `Hello ${name}`;
}
"#;

#[test]
fn plain_class_and_function_nodes_emitted() {
    let provider = TypescriptProvider::new();
    let unit = provider
        .parse_file("src/user.service.ts", PLAIN_SAMPLE)
        .expect("should parse");

    let class_nodes: Vec<_> = unit
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Class)
        .collect();
    assert!(!class_nodes.is_empty(), "expected a Class node");
    assert!(
        class_nodes.iter().any(|n| n.name == "UserService"),
        "expected UserService class"
    );

    let fn_nodes: Vec<_> = unit
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Function)
        .collect();
    assert!(!fn_nodes.is_empty(), "expected Function nodes");
    assert!(
        fn_nodes.iter().any(|n| n.name == "greet"),
        "expected greet function"
    );
}

const EXPRESS_SAMPLE: &str = r#"
const express = require('express');
const app = express();

app.get('/users', (req, res) => {
res.json([]);
});

app.post('/users', (req, res) => {
res.status(201).json({});
});
"#;

#[test]
fn express_router_routes_emitted() {
    let provider = TypescriptProvider::new();
    let unit = provider
        .parse_file("src/server.ts", EXPRESS_SAMPLE)
        .expect("should parse");
    let routes: Vec<_> = unit
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Route)
        .collect();
    assert!(!routes.is_empty(), "expected Express route nodes");
    let names: Vec<&str> = routes.iter().map(|n| n.name.as_str()).collect();
    assert!(
        names.iter().any(|n| n.contains("GET")),
        "expected GET /users, got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("POST")),
        "expected POST /users, got: {names:?}"
    );
}

// ── Outbound HTTP contract sites (Phase C: fetch / axios) ───────────────────

fn ts_contract_sites(src: &str) -> Vec<cih_core::ContractSite> {
    TypescriptProvider::new()
        .parse_file("src/client.ts", src)
        .expect("should parse")
        .parsed_file
        .contract_sites
}

#[test]
fn bare_fetch_is_http_call_get() {
    let src = r#"
export async function loadOrders() {
    const res = await fetch('/api/orders');
    return res.json();
}
"#;
    let sites = ts_contract_sites(src);
    assert_eq!(sites.len(), 1, "expected one site, got {sites:?}");
    let site = &sites[0];
    assert_eq!(site.kind, cih_core::ContractKind::HttpCall);
    assert_eq!(site.http_method.as_deref(), Some("GET"));
    assert_eq!(site.url_template.as_deref(), Some("/api/orders"));
    assert_eq!(
        site.in_callable.as_str(),
        "Function:src/client#loadOrders/0"
    );
}

#[test]
fn fetch_with_method_option() {
    let src = r#"
export function createOrder(body: any) {
    return fetch('/api/orders', { method: 'POST', body: JSON.stringify(body) });
}
"#;
    let sites = ts_contract_sites(src);
    assert_eq!(sites[0].http_method.as_deref(), Some("POST"));
}

#[test]
fn axios_verb_call_is_http_call() {
    let src = r#"
export function loadOrder(id: string) {
    return axios.get('https://orders.internal/api/orders/1');
}
"#;
    let sites = ts_contract_sites(src);
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].http_method.as_deref(), Some("GET"));
    // host is stripped, path kept
    assert_eq!(sites[0].url_template.as_deref(), Some("/api/orders/1"));
}

#[test]
fn template_url_yields_dynamic_parts() {
    use cih_core::UrlPart;
    let src = r#"
export function loadOrder(id: string) {
    return fetch(`/api/orders/${id}`);
}
"#;
    let sites = ts_contract_sites(src);
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].url_template, None);
    assert_eq!(
        sites[0].url_parts.as_deref(),
        Some(&[UrlPart::Lit("/api/orders/".into()), UrlPart::Dynamic][..])
    );
}

#[test]
fn module_level_fetch_falls_back_to_file_caller() {
    // Arrow functions are not tracked as callables v1; the file-id fallback is
    // pinned because it degrades trace_flow_x entry resolution, not just
    // display granularity.
    let src = r#"
const preload = fetch('/api/config');
"#;
    let sites = ts_contract_sites(src);
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].in_callable.as_str(), "File:src/client.ts");
}

#[test]
fn instance_clients_are_not_emitted() {
    let src = r#"
export function load(myobj: any, http: any) {
    myobj.get('/api/orders');
    this.http.get('/api/orders');
    notaxios.post('/api/orders');
}
"#;
    assert!(ts_contract_sites(src).is_empty());
}
