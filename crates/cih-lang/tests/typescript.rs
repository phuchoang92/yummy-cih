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

// ── URL constants + template-substitution ConstRefs (review-finding F2) ─────

fn ts_string_constants(src: &str) -> Vec<cih_core::StringConstant> {
    TypescriptProvider::new()
        .parse_file("src/services/apiClient.ts", src)
        .expect("should parse")
        .parsed_file
        .string_constants
}

#[test]
fn template_substitution_identifier_becomes_const_ref() {
    let src = r#"
const API_BASE_URL = '/api';
export async function load(id: string) {
    return fetch(`${API_BASE_URL}/admin/x`);
}
"#;
    let sites = ts_contract_sites(src);
    assert_eq!(sites.len(), 1);
    let parts = sites[0].url_parts.as_ref().expect("parts");
    assert_eq!(
        parts,
        &vec![
            cih_core::UrlPart::ConstRef("API_BASE_URL".into()),
            cih_core::UrlPart::Lit("/admin/x".into()),
        ]
    );
}

#[test]
fn template_substitution_member_expression_stays_dynamic() {
    let src = r#"
export async function load(cfg: any) {
    return fetch(`${cfg.base}/x`);
}
"#;
    let sites = ts_contract_sites(src);
    let parts = sites[0].url_parts.as_ref().expect("parts");
    assert!(matches!(parts[0], cih_core::UrlPart::Dynamic));
}

#[test]
fn module_const_string_emits_constant() {
    let constants = ts_string_constants("export const API_BASE_URL = '/api/v1';\n");
    assert_eq!(constants.len(), 1);
    assert_eq!(constants[0].const_name, "API_BASE_URL");
    assert_eq!(constants[0].owner_fqcn, "src/services/apiClient");
    assert_eq!(constants[0].value, "/api/v1");
    assert!(!constants[0].dynamic);
    assert!(!constants[0].env_default);
}

#[test]
fn env_default_initializers_emit_default_literal() {
    // The real-world shape: env override with a literal default.
    let nullish = ts_string_constants(
        "export const API_BASE_URL = import.meta.env.VITE_API_URL ?? '/api/v1';\n",
    );
    assert_eq!(nullish.len(), 1);
    assert_eq!(nullish[0].value, "/api/v1");
    assert!(nullish[0].env_default);

    let logical_or = ts_string_constants("const BASE = process.env.BASE || '/api';\n");
    assert_eq!(logical_or.len(), 1);
    assert_eq!(logical_or[0].value, "/api");
    assert!(logical_or[0].env_default);
}

#[test]
fn non_literal_and_non_const_declarations_emit_nothing() {
    assert!(ts_string_constants("export const X = getBase();\n").is_empty());
    assert!(ts_string_constants("let X = '/x';\n").is_empty());
    assert!(
        ts_string_constants("export function f() { const X = '/inner'; return X; }\n").is_empty()
    );
}

// ── HTTP wrapper detection + provisional call sites ─────────────────────────

fn ts_http_wrappers(src: &str) -> Vec<cih_core::HttpWrapperDef> {
    TypescriptProvider::new()
        .parse_file("src/services/apiClient.ts", src)
        .expect("should parse")
        .parsed_file
        .http_wrappers
}

const API_CLIENT_SHAPE: &str = r#"
export const API_BASE_URL = import.meta.env.VITE_API_URL ?? '/api/v1';

export const apiFetch = async <T = unknown>(endpoint: string, options: any = {}, token?: string): Promise<T> => {
    if (/^https?:\/\//i.test(endpoint)) {
        throw new Error('relative paths only');
    }
    const url = `${API_BASE_URL}${endpoint}`;
    try {
        const response = await fetch(url, { ...options });
        return response.json();
    } catch (e) {
        throw e;
    }
};
"#;

#[test]
fn wrapper_def_detected_via_local_url_indirection() {
    let wrappers = ts_http_wrappers(API_CLIENT_SHAPE);
    assert_eq!(wrappers.len(), 1, "wrappers: {wrappers:?}");
    let w = &wrappers[0];
    assert_eq!(w.name, "apiFetch");
    assert_eq!(w.module, "src/services/apiClient");
    assert_eq!(
        w.prefix_parts,
        vec![cih_core::UrlPart::ConstRef("API_BASE_URL".into())]
    );
    assert_eq!(w.options_arg_index, 1);
}

#[test]
fn wrapper_def_function_declaration_form() {
    let wrappers = ts_http_wrappers(
        r#"
const BASE = '/api';
export function apiGet(path: string) {
    return fetch(`${BASE}${path}`);
}
"#,
    );
    assert_eq!(wrappers.len(), 1);
    assert_eq!(wrappers[0].name, "apiGet");
}

#[test]
fn wrapper_rejections() {
    // Param mid-URL.
    assert!(
        ts_http_wrappers("export const f = (p: string) => fetch(`/a/${p}/suffix`);\n").is_empty()
    );
    // No inner fetch.
    assert!(ts_http_wrappers("export const f = (p: string) => compute(p);\n").is_empty());
    // Destructured first param.
    assert!(
        ts_http_wrappers("export const f = ({ path }: any) => fetch(`/x${path}`);\n").is_empty()
    );
    // Ambiguous const url (two declarators across branches).
    assert!(ts_http_wrappers(
        r#"
export const f = (p: string, flag: boolean) => {
    if (flag) {
        const url = `/a${p}`;
        return fetch(url);
    } else {
        const url = `/b${p}`;
        return fetch(url);
    }
};
"#
    )
    .is_empty());
    // Closure must not fake a wrapper.
    assert!(ts_http_wrappers(
        r#"
export const f = (p: string) => {
    return () => fetch(`/x${p}`);
};
"#
    )
    .is_empty());
}

#[test]
fn provisional_sites_for_wrapper_calls() {
    let sites = ts_contract_sites(
        r#"
import { apiFetch } from './apiClient';
export const create = (body: any, token: string) =>
    apiFetch('/admin/llm/providers', { method: 'POST' }, token);
export const logs = (id: number, token: string) =>
    apiFetch(`/admin/activity-logs/${id}`, {}, token);
"#,
    );
    assert_eq!(sites.len(), 2, "sites: {sites:?}");
    assert_eq!(sites[0].via_wrapper.as_deref(), Some("apiFetch"));
    assert_eq!(sites[0].http_method.as_deref(), Some("POST"));
    assert_eq!(
        sites[0].url_parts.as_deref(),
        Some(&[cih_core::UrlPart::Lit("/admin/llm/providers".into())][..])
    );
    assert_eq!(
        sites[0].url_template, None,
        "wrapper sites always carry parts"
    );
    assert_eq!(sites[1].http_method.as_deref(), Some("GET"));
    assert_eq!(
        sites[1].url_parts.as_deref(),
        Some(
            &[
                cih_core::UrlPart::Lit("/admin/activity-logs/".into()),
                cih_core::UrlPart::Dynamic
            ][..]
        )
    );
}

#[test]
fn provisional_not_emitted_for_non_url_args() {
    let sites = ts_contract_sites(
        r#"
export const f = (id: string) => {
    t('common.title');
    helper(id);
    navigate(computePath());
};
"#,
    );
    assert!(sites.is_empty(), "sites: {sites:?}");
}

#[test]
fn wrapper_inner_fetch_stays_unresolvable() {
    // The wrapper's own fetch(url, …) folds to a bare local → all-{*} at
    // resolve → dropped. Pin the parse-side shape here.
    let sites = ts_contract_sites(API_CLIENT_SHAPE);
    assert_eq!(sites.len(), 1);
    assert!(sites[0].via_wrapper.is_none());
    // The bare local becomes ConstRef("url") — lowercase, so the resolver's
    // convention gate blocks cross-file lookup and (with no same-module
    // constant named `url`) it stays unresolved → all-{*} → dropped.
    assert_eq!(
        sites[0].url_parts.as_deref(),
        Some(&[cih_core::UrlPart::ConstRef("url".into())][..])
    );
}

#[test]
fn namespace_import_records_alias() {
    let imports = TypescriptProvider::new()
        .parse_file(
            "src/svc.ts",
            "import * as api from './apiClient';\nimport def from './d';\nimport { named } from './n';\n",
        )
        .expect("should parse")
        .parsed_file
        .imports;
    let pairs: Vec<(String, Option<String>)> = imports
        .iter()
        .map(|imp| (imp.raw.clone(), imp.alias.clone()))
        .collect();
    // Each import keeps its module-path RawImport (namespace records the alias);
    // relative default/named imports also emit a module-qualified RawImport
    // (`<resolved-module>.<Local>`) so `build_import_map` can key the symbol.
    assert_eq!(
        pairs,
        vec![
            ("./apiClient".to_string(), Some("api".to_string())),
            ("./d".to_string(), None),
            ("src/d.def".to_string(), None),
            ("./n".to_string(), None),
            ("src/n.named".to_string(), None),
        ]
    );
}

#[test]
fn ts_provisional_sites_for_namespace_alias_calls() {
    let sites = ts_contract_sites(
        r#"
import * as api from './apiClient';
export const create = (body: any) =>
    api.apiFetch('/admin/x', { method: 'POST' }, body);
"#,
    );
    assert_eq!(sites.len(), 1, "{sites:?}");
    assert_eq!(sites[0].via_wrapper.as_deref(), Some("api.apiFetch"));
    assert_eq!(sites[0].http_method.as_deref(), Some("POST"));
}

#[test]
fn ts_named_import_member_call_not_emitted() {
    // Named imports carry no alias — member calls on them stay out (the
    // named-import bare-callee path already covers `apiFetch(...)` directly).
    let sites = ts_contract_sites(
        r#"
import { api } from './x';
export const f = () => api.get('/y');
"#,
    );
    assert!(sites.is_empty(), "{sites:?}");
}
