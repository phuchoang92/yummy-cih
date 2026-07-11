use cih_core::{ContractKind, ContractSite, EdgeKind, NodeKind, UrlPart};
use cih_lang::{go::GoProvider, LanguageProvider};

fn parse(src: &str) -> cih_core::ParsedUnit {
    GoProvider::new()
        .parse_file("cmd/server/main.go", src)
        .expect("sample should parse")
}

fn routes(src: &str) -> Vec<cih_core::Node> {
    parse(src)
        .nodes
        .into_iter()
        .filter(|n| n.kind == NodeKind::Route)
        .collect()
}

fn contract_sites(src: &str) -> Vec<ContractSite> {
    parse(src).parsed_file.contract_sites
}

// ── Routes ───────────────────────────────────────────────────────────────────

#[test]
fn net_http_handlefunc_registers_any_route_with_handler() {
    let src = r#"package main

import "net/http"

func handleOrders(w http.ResponseWriter, r *http.Request) {}

func main() {
    http.HandleFunc("/orders", handleOrders)
}
"#;
    let unit = parse(src);
    let route = unit
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::Route)
        .expect("route node");
    assert_eq!(route.id.as_str(), "Route:go:ANY:/orders");
    let props = route.props.as_ref().unwrap();
    assert_eq!(props["httpMethod"], "ANY");
    assert_eq!(props["path"], "/orders");
    assert_eq!(props["source"], "go");

    let handles: Vec<_> = unit
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::HandlesRoute)
        .collect();
    assert_eq!(handles.len(), 1);
    assert_eq!(
        handles[0].src.as_str(),
        "Function:main.handleOrders#handleOrders/2"
    );
}

#[test]
fn go_122_method_pattern_splits_verb() {
    let src = r#"package main

import "net/http"

func main() {
    http.HandleFunc("GET /orders/{id}", nil)
}
"#;
    let routes = routes(src);
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].id.as_str(), "Route:go:GET:/orders/{id}");
    assert_eq!(routes[0].props.as_ref().unwrap()["httpMethod"], "GET");
}

#[test]
fn gin_verb_route() {
    let src = r#"package main

import "github.com/gin-gonic/gin"

func main() {
    r := gin.Default()
    r.POST("/api/orders", createOrder)
}
"#;
    let routes = routes(src);
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].id.as_str(), "Route:go:POST:/api/orders");
}

#[test]
fn chi_capitalized_verb_route() {
    let src = r#"package main

import "github.com/go-chi/chi/v5"

func main() {
    r := chi.NewRouter()
    r.Get("/api/items", nil)
}
"#;
    let routes = routes(src);
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].id.as_str(), "Route:go:GET:/api/items");
}

#[test]
fn no_http_import_means_no_routes() {
    // Same shapes, no gated import: the map-like `Get` and a `HandleFunc`
    // on some custom type must not become routes.
    let src = r#"package main

func main() {
    cache.Get("/api/items")
    bus.HandleFunc("/orders", handler)
}
"#;
    assert!(routes(src).is_empty());
    assert!(contract_sites(src).is_empty());
}

#[test]
fn chi_verbs_require_chi_import() {
    // net/http alone must not make `cache.Get("/key")` a route.
    let src = r#"package main

import "net/http"

func main() {
    cache.Get("/api/items")
}
"#;
    assert!(routes(src).is_empty());
}

// ── Outbound ─────────────────────────────────────────────────────────────────

#[test]
fn http_get_is_http_call() {
    let src = r#"package main

import "net/http"

func load() {
    http.Get("https://orders.internal/api/orders")
}
"#;
    let sites = contract_sites(src);
    assert_eq!(sites.len(), 1, "expected one site, got {sites:?}");
    assert_eq!(sites[0].kind, ContractKind::HttpCall);
    assert_eq!(sites[0].http_method.as_deref(), Some("GET"));
    assert_eq!(sites[0].url_template.as_deref(), Some("/api/orders"));
    assert_eq!(sites[0].in_callable.as_str(), "Function:main.load#load/0");
}

#[test]
fn new_request_takes_method_from_literal() {
    let src = r#"package main

import "net/http"

func send() {
    req, _ := http.NewRequest("POST", "/api/orders", nil)
    _ = req
}
"#;
    let sites = contract_sites(src);
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].http_method.as_deref(), Some("POST"));
    assert_eq!(sites[0].url_template.as_deref(), Some("/api/orders"));
}

#[test]
fn sprintf_url_yields_parts() {
    let src = r#"package main

import (
    "fmt"
    "net/http"
)

func load(id int) {
    http.Get(fmt.Sprintf("/api/orders/%d/items", id))
}
"#;
    let sites = contract_sites(src);
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].url_template, None);
    assert_eq!(
        sites[0].url_parts.as_deref(),
        Some(
            &[
                UrlPart::Lit("/api/orders/".into()),
                UrlPart::Dynamic,
                UrlPart::Lit("/items".into()),
            ][..]
        )
    );
}

#[test]
fn concat_url_yields_const_ref_parts() {
    let src = r#"package main

import "net/http"

func load() {
    http.Get(baseURL + "/orders")
}
"#;
    let sites = contract_sites(src);
    assert_eq!(
        sites[0].url_parts.as_deref(),
        Some(
            &[
                UrlPart::ConstRef("baseURL".into()),
                UrlPart::Lit("/orders".into()),
            ][..]
        )
    );
}

#[test]
fn client_do_is_skipped() {
    let src = r#"package main

import "net/http"

func send(client *http.Client, req *http.Request) {
    client.Do(req)
}
"#;
    assert!(contract_sites(src).is_empty());
}
