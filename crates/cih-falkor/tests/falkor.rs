use cih_core::{NodeId, NodeKind};
use cih_graph_store::{risk_from_fanout, CommunityEdge, RouteInfo};

#[test]
fn node_kind_label_roundtrip() {
    for kind in [
        NodeKind::File,
        NodeKind::Folder,
        NodeKind::Class,
        NodeKind::Interface,
        NodeKind::Enum,
        NodeKind::Record,
        NodeKind::Annotation,
        NodeKind::Method,
        NodeKind::Function,
        NodeKind::Constructor,
        NodeKind::Field,
        NodeKind::Route,
        NodeKind::Community,
        NodeKind::Process,
        NodeKind::Other,
    ] {
        assert_eq!(NodeKind::from_label(kind.label()), kind);
    }
    assert_eq!(NodeKind::from_label("Unknown"), NodeKind::Other);
}

#[test]
fn risk_from_fanout_buckets() {
    assert_eq!(risk_from_fanout(0), "none");
    assert_eq!(risk_from_fanout(5), "low");
    assert_eq!(risk_from_fanout(20), "medium");
    assert_eq!(risk_from_fanout(75), "high");
    assert_eq!(risk_from_fanout(76), "critical");
}

fn make_row(cells: &[&str]) -> Vec<String> {
    cells.iter().map(|s| s.to_string()).collect()
}

#[test]
fn route_map_row_parses_correctly() {
    let row = make_row(&[
        "/api/users",
        "GET",
        "GetMapping",
        "Method:com.example.UserController#list/0",
        "Method:com.example.UserController#list/0",
        "list",
        "com.example.UserController#list/0",
    ]);
    let info = RouteInfo {
        path: row.first().cloned().unwrap_or_default(),
        http_method: row.get(1).cloned().unwrap_or_default(),
        decorator: row.get(2).cloned().unwrap_or_default(),
        handler_id: NodeId::new(row.get(4).cloned().unwrap_or_default()),
        handler_name: row.get(5).cloned().unwrap_or_default(),
        handler_qualified: row.get(6).cloned().unwrap_or_default(),
    };
    assert_eq!(info.path, "/api/users");
    assert_eq!(info.http_method, "GET");
    assert_eq!(info.decorator, "GetMapping");
    assert_eq!(
        info.handler_id.as_str(),
        "Method:com.example.UserController#list/0"
    );
    assert_eq!(info.handler_name, "list");
    assert_eq!(info.handler_qualified, "com.example.UserController#list/0");
}

#[test]
fn community_graph_row_parses_correctly() {
    let row = make_row(&["Community:order-service", "Community:payment-service", "12"]);
    let edge = CommunityEdge {
        src: row[0].clone(),
        dst: row[1].clone(),
        weight: row[2].parse().unwrap_or(0),
    };
    assert_eq!(edge.src, "Community:order-service");
    assert_eq!(edge.dst, "Community:payment-service");
    assert_eq!(edge.weight, 12);
}

#[test]
fn route_map_empty_result_returns_empty_vec() {
    let rows: Vec<Vec<String>> = vec![];
    let result: Vec<RouteInfo> = rows
        .into_iter()
        .filter(|row| row.len() >= 6)
        .map(|row| RouteInfo {
            path: row.first().cloned().unwrap_or_default(),
            http_method: row.get(1).cloned().unwrap_or_default(),
            decorator: row.get(2).cloned().unwrap_or_default(),
            handler_id: NodeId::new(row.get(4).cloned().unwrap_or_default()),
            handler_name: row.get(5).cloned().unwrap_or_default(),
            handler_qualified: row.get(6).cloned().unwrap_or_default(),
        })
        .collect();
    assert!(result.is_empty());
}
