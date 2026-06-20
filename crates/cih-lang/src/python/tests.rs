use super::*;
use cih_core::NodeKind;

const FLASK_SAMPLE: &str = r#"
from flask import Flask

app = Flask(__name__)

@app.route('/users', methods=['GET', 'POST'])
def list_users():
return []

@app.get('/users/<int:id>')
def get_user(id):
return {}
"#;

#[test]
fn flask_route_decorator_emits_route_nodes() {
    let provider = PythonProvider::new();
    let unit = provider
        .parse_file("src/orders/views.py", FLASK_SAMPLE)
        .expect("should parse");
    let routes: Vec<_> = unit
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Route)
        .collect();
    assert!(!routes.is_empty(), "expected Flask route nodes, got 0");
    let names: Vec<&str> = routes.iter().map(|n| n.name.as_str()).collect();
    // @app.route with methods=['GET', 'POST'] → 2 route nodes
    assert!(
        names.iter().any(|n| n.contains("GET")),
        "expected a GET route: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("POST")),
        "expected a POST route: {names:?}"
    );
    for route in &routes {
        let props = route.props.as_ref().expect("route has props");
        assert!(
            props["source"].as_str().unwrap_or("").contains("flask"),
            "source should be flask, got: {props}"
        );
    }
}

const FASTAPI_SAMPLE: &str = r#"
from fastapi import APIRouter

router = APIRouter()

@router.get('/items')
def list_items():
return []

@router.post('/items')
def create_item(body: dict):
return {}
"#;

#[test]
fn fastapi_route_decorator_emits_route_nodes() {
    let provider = PythonProvider::new();
    let unit = provider
        .parse_file("src/items/router.py", FASTAPI_SAMPLE)
        .expect("should parse");
    let routes: Vec<_> = unit
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Route)
        .collect();
    assert!(!routes.is_empty(), "expected FastAPI route nodes");
    let names: Vec<&str> = routes.iter().map(|n| n.name.as_str()).collect();
    assert!(
        names.iter().any(|n| n.contains("GET")),
        "expected GET route: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("POST")),
        "expected POST route: {names:?}"
    );
    for route in &routes {
        let props = route.props.as_ref().expect("route has props");
        assert!(
            props["source"].as_str().unwrap_or("").contains("fast_api"),
            "source should be fast_api, got: {props}"
        );
    }
}

const CLASS_SAMPLE: &str = r#"
class OrderService:
def find_order(self, order_id: int):
    return None

def create_order(self, data: dict):
    return {}

def standalone_fn(x: int) -> int:
return x + 1
"#;

#[test]
fn plain_class_and_methods_emitted() {
    let provider = PythonProvider::new();
    let unit = provider
        .parse_file("src/orders/service.py", CLASS_SAMPLE)
        .expect("should parse");

    let class_nodes: Vec<_> = unit
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Class)
        .collect();
    assert!(!class_nodes.is_empty(), "expected a Class node");
    assert!(
        class_nodes.iter().any(|n| n.name == "OrderService"),
        "expected OrderService class"
    );

    let fn_nodes: Vec<_> = unit
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Function)
        .collect();
    assert!(!fn_nodes.is_empty(), "expected Function nodes");
    assert!(
        fn_nodes.iter().any(|n| n.name == "standalone_fn"),
        "expected standalone_fn: {:?}",
        fn_nodes.iter().map(|n| &n.name).collect::<Vec<_>>()
    );
    assert!(
        fn_nodes.iter().any(|n| n.name == "find_order"),
        "expected find_order method"
    );
}
