use cih_core::{NodeKind, RefKind};
use cih_lang::python::parse::parse_python_file;

fn route_names_for(src: &str) -> Vec<String> {
    let unit = parse_python_file("test.py", src).unwrap();
    unit.nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Route)
        .map(|n| n.name.clone())
        .collect()
}

/// (path, source) for each Route node.
fn route_sources_for(src: &str) -> Vec<(String, String)> {
    let unit = parse_python_file("test.py", src).unwrap();
    let mut out: Vec<(String, String)> = unit
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Route)
        .map(|n| {
            let props = n.props.as_ref().unwrap();
            (
                props["path"].as_str().unwrap_or("").to_string(),
                props["source"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn call_reference_attributed_to_enclosing_function() {
    // A call to `helper` inside `main` must record `main` as the caller (in_callable is main's
    // Function node id, in_fqcn is main's signature) so a CALLS edge originates from the function,
    // not the file.
    let src = r#"
def helper(x):
    return x

def main():
    return helper(5)
"#;
    let unit = parse_python_file("test.py", src).unwrap();
    let call = unit
        .parsed_file
        .reference_sites
        .iter()
        .find(|r| r.kind == RefKind::Call && r.name == "helper")
        .expect("call reference to helper");
    assert_eq!(call.in_fqcn, "test#main/0");
    assert_eq!(call.in_callable.as_str(), "Function:test#main/0");
}

#[test]
fn nested_function_call_attributed_to_inner_function() {
    let src = r#"
def outer():
    def inner():
        return helper()
    return inner
"#;
    let unit = parse_python_file("test.py", src).unwrap();
    let call = unit
        .parsed_file
        .reference_sites
        .iter()
        .find(|r| r.kind == RefKind::Call && r.name == "helper")
        .expect("call reference to helper");
    assert_eq!(call.in_callable.as_str(), "Function:test#inner/0");
}

#[test]
fn module_level_call_falls_back_to_file() {
    // A call at module scope (no enclosing function) stays file-attributed.
    let src = "configure()\n";
    let unit = parse_python_file("test.py", src).unwrap();
    let call = unit
        .parsed_file
        .reference_sites
        .iter()
        .find(|r| r.kind == RefKind::Call && r.name == "configure")
        .expect("call reference to configure");
    assert_eq!(call.in_callable.as_str(), "File:test.py");
}

#[test]
fn app_get_labeled_fastapi_when_fastapi_imported() {
    let src = r#"
from fastapi import FastAPI
app = FastAPI()

@app.get("/livez")
def livez():
    return {}
"#;
    assert_eq!(
        route_sources_for(src),
        vec![("/livez".to_string(), "fast_api".to_string())]
    );
}

#[test]
fn app_get_labeled_flask_when_flask_imported() {
    // Flask 2.0+ also supports @app.get shorthand — must stay flask when the file imports flask.
    let src = r#"
from flask import Flask
app = Flask(__name__)

@app.get("/livez")
def livez():
    return {}
"#;
    assert_eq!(
        route_sources_for(src),
        vec![("/livez".to_string(), "flask".to_string())]
    );
}

#[test]
fn fastapi_router_prefix_is_composed() {
    let src = r#"
from fastapi import APIRouter

router = APIRouter(prefix="/orders")

@router.get("/list")
def list_orders():
pass

@router.post("/create")
def create_order():
pass
"#;
    let mut names = route_names_for(src);
    names.sort();
    assert_eq!(names, vec!["GET /orders/list", "POST /orders/create"]);
}

#[test]
fn flask_blueprint_prefix_is_composed() {
    let src = r#"
from flask import Blueprint

orders_bp = Blueprint("orders", __name__, url_prefix="/api/orders")

@orders_bp.route("/list", methods=["GET", "POST"])
def list_orders():
pass
"#;
    let mut names = route_names_for(src);
    names.sort();
    assert_eq!(names, vec!["GET /api/orders/list", "POST /api/orders/list"]);
}

#[test]
fn plain_app_routes_unaffected() {
    let src = r#"
from flask import Flask
app = Flask(__name__)

@app.route("/health")
def health():
pass
"#;
    let names = route_names_for(src);
    assert_eq!(names, vec!["GET /health"]);
}

// ── Outbound HTTP contract sites (Phase C: requests / httpx) ────────────────

fn py_contract_sites(src: &str) -> Vec<cih_core::ContractSite> {
    parse_python_file("app/client.py", src)
        .unwrap()
        .parsed_file
        .contract_sites
}

#[test]
fn requests_verb_call_is_http_call() {
    let src = r#"
import requests

def load_orders():
    return requests.get("/api/orders")
"#;
    let sites = py_contract_sites(src);
    assert_eq!(sites.len(), 1, "expected one site, got {sites:?}");
    let site = &sites[0];
    assert_eq!(site.kind, cih_core::ContractKind::HttpCall);
    assert_eq!(site.http_method.as_deref(), Some("GET"));
    assert_eq!(site.url_template.as_deref(), Some("/api/orders"));
    assert_eq!(
        site.in_callable.as_str(),
        "Function:app.client#load_orders/0"
    );
}

#[test]
fn httpx_post_and_requests_request() {
    let src = r#"
import httpx
import requests

def create():
    httpx.post("https://orders.internal/api/orders")
    requests.request("PUT", "/api/orders/1")
"#;
    let sites = py_contract_sites(src);
    assert_eq!(sites.len(), 2);
    assert_eq!(sites[0].http_method.as_deref(), Some("POST"));
    assert_eq!(sites[0].url_template.as_deref(), Some("/api/orders"));
    assert_eq!(sites[1].http_method.as_deref(), Some("PUT"));
    assert_eq!(sites[1].url_template.as_deref(), Some("/api/orders/1"));
}

#[test]
fn fstring_url_yields_dynamic_parts() {
    use cih_core::UrlPart;
    let src = r#"
import requests

def load(order_id):
    return requests.get(f"/api/orders/{order_id}")
"#;
    let sites = py_contract_sites(src);
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].url_template, None);
    assert_eq!(
        sites[0].url_parts.as_deref(),
        Some(&[UrlPart::Lit("/api/orders/".into()), UrlPart::Dynamic][..])
    );
}

#[test]
fn module_level_call_falls_back_to_file_caller() {
    // Pinned: a file-id in_callable degrades trace_flow_x entry resolution
    // (the first leg), not just display granularity.
    let src = r#"
import requests

CONFIG = requests.get("/api/config")
"#;
    let sites = py_contract_sites(src);
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].in_callable.as_str(), "File:app/client.py");
}

#[test]
fn non_module_receivers_are_not_emitted() {
    let src = r#"
def load(session, myobj):
    session.get("/api/orders")
    myobj.get("/api/orders")
    client.requests.get("/api/orders")
"#;
    assert!(py_contract_sites(src).is_empty());
}

// ── URL constants + f-string ConstRefs (review-finding F2) ──────────────────

fn py_string_constants(src: &str) -> Vec<cih_core::StringConstant> {
    parse_python_file("src/app/client.py", src)
        .expect("should parse")
        .parsed_file
        .string_constants
}

fn py_sites(src: &str) -> Vec<cih_core::ContractSite> {
    parse_python_file("src/app/client.py", src)
        .expect("should parse")
        .parsed_file
        .contract_sites
}

#[test]
fn fstring_screaming_snake_becomes_const_ref() {
    let sites = py_sites("import requests\n\ndef load(item_id):\n    return requests.get(f\"{API_BASE}/items/{item_id}\")\n");
    assert_eq!(sites.len(), 1);
    let parts = sites[0].url_parts.as_ref().expect("parts");
    assert_eq!(parts[0], cih_core::UrlPart::ConstRef("API_BASE".into()));
    // the lowercase local stays Dynamic
    assert!(parts
        .iter()
        .any(|p| matches!(p, cih_core::UrlPart::Dynamic)));
}

#[test]
fn fstring_attribute_stays_dynamic() {
    let sites = py_sites(
        "import requests\n\ndef load(settings):\n    return requests.get(f\"{settings.base}/x\")\n",
    );
    let parts = sites[0].url_parts.as_ref().expect("parts");
    assert!(matches!(parts[0], cih_core::UrlPart::Dynamic));
}

#[test]
fn module_constants_plain_and_env_default_forms() {
    let plain = py_string_constants("API_BASE = \"/api/v1\"\n");
    assert_eq!(plain.len(), 1);
    assert_eq!(plain[0].owner_fqcn, "src.app.client");
    assert_eq!(plain[0].value, "/api/v1");
    assert!(!plain[0].env_default);

    let or_form = py_string_constants("API_BASE = base or \"/api/v1\"\n");
    assert_eq!(or_form.len(), 1);
    assert_eq!(or_form[0].value, "/api/v1");
    assert!(or_form[0].env_default);

    let environ =
        py_string_constants("import os\nAPI_BASE = os.environ.get(\"API_URL\", \"/api/v1\")\n");
    assert_eq!(environ.len(), 1);
    assert_eq!(environ[0].value, "/api/v1");
    assert!(environ[0].env_default);

    let getenv = py_string_constants("import os\nAPI_BASE = os.getenv(\"API_URL\", \"/api/v1\")\n");
    assert_eq!(getenv.len(), 1);
    assert!(getenv[0].env_default);
}

#[test]
fn computed_and_function_scoped_assignments_emit_nothing() {
    assert!(py_string_constants("API_BASE = compute()\n").is_empty());
    assert!(
        py_string_constants("def f():\n    API_BASE = \"/x\"\n    return API_BASE\n").is_empty()
    );
}

// ── Dotted import recording (review: python cross-file resolution bugfix) ────

fn py_import_raws(rel: &str, src: &str) -> Vec<(String, bool)> {
    parse_python_file(rel, src)
        .expect("should parse")
        .parsed_file
        .imports
        .iter()
        .map(|imp| (imp.raw.clone(), imp.is_wildcard))
        .collect()
}

#[test]
fn python_imports_record_dotted_modules() {
    let raws = py_import_raws(
        "services/sub/mod.py",
        "import a.b\nimport a.b as c\nimport os, sys\nfrom a.b import x, y\nfrom a.b import *\n",
    );
    assert_eq!(
        raws,
        vec![
            ("a.b".to_string(), false),
            ("a.b".to_string(), false),
            ("os".to_string(), false),
            ("sys".to_string(), false),
            ("a.b".to_string(), false),
            ("a.b".to_string(), true),
        ]
    );
}

#[test]
fn python_relative_imports_normalize_against_file_dir() {
    let raws = py_import_raws(
        "services/sub/mod.py",
        "from .api_client import api_get\nfrom ..pkg import w\nfrom . import z\n",
    );
    assert_eq!(
        raws.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>(),
        vec!["services.sub.api_client", "services.pkg", "services.sub"]
    );
}

// ── HTTP wrapper detection + provisional call sites ─────────────────────────

fn py_http_wrappers(src: &str) -> Vec<cih_core::HttpWrapperDef> {
    parse_python_file("services/api_client.py", src)
        .expect("should parse")
        .parsed_file
        .http_wrappers
}

const PY_API_CLIENT: &str = r#"import os
import requests

API_BASE = os.environ.get("API_URL", "/api/v1")

def api_get(path):
    url = f"{API_BASE}{path}"
    return requests.get(url)

def api_post(path, data):
    return requests.post(API_BASE + path, json=data)

def api_call(path):
    return requests.request("POST", API_BASE + path)

def passthrough(url):
    return requests.get(url)
"#;

#[test]
fn py_wrapper_defs_detected() {
    let wrappers = py_http_wrappers(PY_API_CLIENT);
    let by_name: std::collections::HashMap<_, _> =
        wrappers.iter().map(|w| (w.name.as_str(), w)).collect();
    assert_eq!(wrappers.len(), 4, "{wrappers:?}");

    let get = by_name["api_get"];
    assert_eq!(get.module, "services.api_client");
    assert_eq!(
        get.prefix_parts,
        vec![cih_core::UrlPart::ConstRef("API_BASE".into())]
    );
    assert_eq!(get.fixed_method.as_deref(), Some("GET"));

    // json= kwarg must not shift the URL arg.
    let post = by_name["api_post"];
    assert_eq!(post.fixed_method.as_deref(), Some("POST"));
    assert_eq!(
        post.prefix_parts,
        vec![cih_core::UrlPart::ConstRef("API_BASE".into())]
    );

    // requests.request("POST", url) literal-verb form.
    let call = by_name["api_call"];
    assert_eq!(call.fixed_method.as_deref(), Some("POST"));

    // Pure pass-through: empty prefix.
    let pt = by_name["passthrough"];
    assert!(pt.prefix_parts.is_empty());
    assert_eq!(pt.fixed_method.as_deref(), Some("GET"));
}

#[test]
fn py_wrapper_rejections() {
    // Method with self param inside a class.
    assert!(py_http_wrappers(
        "import requests\nclass C:\n    def get(self, path):\n        return requests.get(\"/api\" + path)\n"
    )
    .is_empty());
    // Nested def (closure) containing the call.
    assert!(py_http_wrappers(
        "import requests\ndef outer(path):\n    def inner():\n        return requests.get(\"/api\" + path)\n    return inner\n"
    )
    .is_empty());
    // Param mid-URL.
    assert!(py_http_wrappers(
        "import requests\ndef f(path):\n    return requests.get(f\"/a/{path}/suffix\")\n"
    )
    .is_empty());
    // No inner http call.
    assert!(py_http_wrappers("def f(path):\n    return compute(path)\n").is_empty());
    // Two url assignments (ambiguous).
    assert!(py_http_wrappers(
        "import requests\ndef f(path, flag):\n    if flag:\n        url = \"/a\" + path\n    else:\n        url = \"/b\" + path\n    return requests.get(url)\n"
    )
    .is_empty());
    // Non-literal request method (pass-through method wrapper) bails.
    assert!(py_http_wrappers(
        "import requests\ndef f(method, path):\n    return requests.request(method, \"/api\" + path)\n"
    )
    .is_empty());
}

#[test]
fn py_decorated_module_scope_def_is_detected() {
    let wrappers = py_http_wrappers(
        "import requests\n\n@retry\ndef api_get(path):\n    return requests.get(\"/api/v1\" + path)\n",
    );
    assert_eq!(wrappers.len(), 1);
    assert_eq!(wrappers[0].name, "api_get");
}

#[test]
fn py_provisional_sites_for_wrapper_calls() {
    let sites = py_sites(
        "from services.api_client import api_get\n\ndef load(item_id):\n    return api_get(f\"/admin/items/{item_id}\")\n\ndef all_items():\n    return api_get(\"/items\")\n",
    );
    assert_eq!(sites.len(), 2, "{sites:?}");
    assert_eq!(sites[0].via_wrapper.as_deref(), Some("api_get"));
    assert_eq!(sites[0].http_method.as_deref(), Some("GET"));
    assert_eq!(
        sites[0].url_parts.as_deref(),
        Some(
            &[
                cih_core::UrlPart::Lit("/admin/items/".into()),
                cih_core::UrlPart::Dynamic
            ][..]
        )
    );
    // All-Lit args still carry parts (resolve must prepend the prefix).
    assert_eq!(
        sites[1].url_parts.as_deref(),
        Some(&[cih_core::UrlPart::Lit("/items".into())][..])
    );
}

#[test]
fn py_provisional_not_emitted_for_non_url_args() {
    let sites = py_sites("def f(x):\n    t(\"common.x\")\n    helper(x)\n");
    assert!(sites.is_empty(), "{sites:?}");
}

#[test]
fn python_imports_record_aliases() {
    let imports = parse_python_file(
        "app/main.py",
        "import services.api_client as api\nimport plain.module\nfrom a.b import x as y\n",
    )
    .expect("should parse")
    .parsed_file
    .imports;
    let pairs: Vec<(String, Option<String>)> = imports
        .iter()
        .map(|imp| (imp.raw.clone(), imp.alias.clone()))
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("services.api_client".to_string(), Some("api".to_string())),
            ("plain.module".to_string(), None),
            // from-import name aliases are deliberately not captured
            ("a.b".to_string(), None),
        ]
    );
}
