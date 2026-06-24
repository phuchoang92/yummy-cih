use cih_core::NodeKind;
use cih_lang::python::parse::parse_python_file;

fn route_names_for(src: &str) -> Vec<String> {
    let unit = parse_python_file("test.py", src).unwrap();
    unit.nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Route)
        .map(|n| n.name.clone())
        .collect()
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
