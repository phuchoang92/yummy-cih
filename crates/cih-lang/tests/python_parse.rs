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
    assert_eq!(route_sources_for(src), vec![("/livez".to_string(), "fast_api".to_string())]);
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
    assert_eq!(route_sources_for(src), vec![("/livez".to_string(), "flask".to_string())]);
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
