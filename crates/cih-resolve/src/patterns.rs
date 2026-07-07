//! Apply user-defined resolve patterns (`cih.patterns.toml`) over the assembled graph.
//!
//! Deterministic, framework-agnostic: for each `[[route]]` rule, methods carrying the declared
//! annotation get a synthesized `Route` node + `HandlesRoute` edge — the exact shape the built-in
//! Spring/JAX-RS detectors emit (`cih-lang/src/java/parse/framework.rs`), so `route_map`,
//! `trace_flow`, and the CXF prefix stitch all work unchanged. Matches the annotation metadata the
//! parser retains on nodes; no source re-parsing, no per-framework code.

use std::collections::HashSet;

use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind};
use cih_patterns::PatternRules;

/// Synthesize graph facts from user pattern rules. No-op when there are no rules.
pub fn apply_pattern_rules(nodes: &mut Vec<Node>, edges: &mut Vec<Edge>, rules: &PatternRules) {
    if rules.routes.is_empty() {
        return;
    }

    // class FQCN → its annotations array (for class-level prefix rules); existing Route ids to dedup.
    let mut class_annotations: std::collections::HashMap<&str, &serde_json::Value> =
        std::collections::HashMap::new();
    let mut existing_routes: HashSet<&str> = HashSet::new();
    for n in nodes.iter() {
        match n.kind {
            NodeKind::Class | NodeKind::Interface | NodeKind::Enum | NodeKind::Record => {
                if let (Some(fqcn), Some(anns)) = (
                    n.qualified_name.as_deref(),
                    n.props.as_ref().and_then(|p| p.get("annotations")),
                ) {
                    class_annotations.insert(fqcn, anns);
                }
            }
            NodeKind::Route => {
                existing_routes.insert(n.id.as_str());
            }
            _ => {}
        }
    }

    let mut new_nodes: Vec<Node> = Vec::new();
    let mut new_edges: Vec<Edge> = Vec::new();
    let mut created: HashSet<String> = HashSet::new();

    for n in nodes.iter() {
        if n.kind != NodeKind::Method {
            continue;
        }
        let Some(anns) = n
            .props
            .as_ref()
            .and_then(|p| p.get("annotations"))
            .and_then(|a| a.as_array())
        else {
            continue;
        };
        // Method qualified_name is the handler FQCN, e.g. "com.acme.Foo#pay/1".
        let handler = n.qualified_name.clone().unwrap_or_default();
        let owner_fqcn = handler.split('#').next().unwrap_or("");

        for rule in &rules.routes {
            let Some(ann) = anns.iter().find(|a| ann_name(a) == Some(rule.annotation.as_str()))
            else {
                continue;
            };
            // A missing path attribute means the handler sits at the controller root (common for
            // index endpoints, e.g. Micronaut's bare `@Get`); the class prefix then supplies the path.
            let path = attr_str(ann, &rule.path_attr).unwrap_or_default();
            let method = rule
                .fixed_method()
                .or_else(|| {
                    rule.method_attr
                        .as_deref()
                        .and_then(|k| attr_str(ann, k))
                        .map(|m| m.to_ascii_uppercase())
                })
                .unwrap_or_else(|| "GET".to_string());

            let prefix = rule.class_prefix_annotation.as_deref().and_then(|cpa| {
                class_annotations
                    .get(owner_fqcn)
                    .and_then(|anns| anns.as_array())
                    .and_then(|arr| arr.iter().find(|a| ann_name(a) == Some(cpa)))
                    .and_then(|a| attr_str(a, &rule.class_prefix_attr))
            });

            let full = join_route(prefix.as_deref(), &path);
            let name = format!("{method} {full}");
            let route_id = format!("Route:{name}");
            // Don't duplicate a route a built-in detector already produced, or one we just made.
            if existing_routes.contains(route_id.as_str()) || !created.insert(route_id.clone()) {
                continue;
            }

            new_nodes.push(Node {
                id: NodeId::new(route_id.clone()),
                kind: NodeKind::Route,
                name: name.clone(),
                qualified_name: Some(name),
                file: n.file.clone(),
                range: n.range,
                props: Some(serde_json::json!({
                    "httpMethod": method,
                    "path": full,
                    "route_annotations": [rule.annotation.clone()],
                    "source": "custom_pattern",
                    "handler": handler,
                })),
            });
            new_edges.push(Edge {
                src: n.id.clone(),
                dst: NodeId::new(route_id),
                kind: EdgeKind::HandlesRoute,
                confidence: 1.0,
                reason: format!("pattern-{}", rule.annotation),
                props: None,
            });
        }
    }

    nodes.extend(new_nodes);
    edges.extend(new_edges);
}

fn ann_name(ann: &serde_json::Value) -> Option<&str> {
    ann.get("name").and_then(|v| v.as_str())
}

/// Read a string attribute from a retained annotation snapshot (`{name, attrs:{...}}`).
fn attr_str(ann: &serde_json::Value, key: &str) -> Option<String> {
    ann.get("attrs")?.get(key).and_then(|v| v.as_str()).map(String::from)
}

/// Join an optional class prefix and a method path into a single normalized route path.
fn join_route(prefix: Option<&str>, path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in [prefix.unwrap_or(""), path] {
        for piece in seg.split('/') {
            if !piece.is_empty() {
                parts.push(piece);
            }
        }
    }
    format!("/{}", parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::Range;
    use cih_patterns::RouteRule;

    fn method_node(fqcn: &str, annotations: serde_json::Value) -> Node {
        Node {
            id: NodeId::new(format!("Method:{fqcn}")),
            kind: NodeKind::Method,
            name: fqcn.rsplit('#').next().unwrap_or(fqcn).to_string(),
            qualified_name: Some(fqcn.to_string()),
            file: "com/acme/Foo.java".to_string(),
            range: Range::default(),
            props: Some(serde_json::json!({ "annotations": annotations })),
        }
    }

    fn class_node(fqcn: &str, annotations: serde_json::Value) -> Node {
        Node {
            id: NodeId::new(format!("Class:{fqcn}")),
            kind: NodeKind::Class,
            name: fqcn.rsplit('.').next().unwrap_or(fqcn).to_string(),
            qualified_name: Some(fqcn.to_string()),
            file: "com/acme/Foo.java".to_string(),
            range: Range::default(),
            props: Some(serde_json::json!({ "annotations": annotations })),
        }
    }

    fn route_rule(annotation: &str, method: Option<&str>) -> RouteRule {
        RouteRule {
            annotation: annotation.to_string(),
            path_attr: "value".to_string(),
            method: method.map(String::from),
            method_attr: None,
            class_prefix_annotation: None,
            class_prefix_attr: "value".to_string(),
        }
    }

    #[test]
    fn synthesizes_route_from_custom_annotation() {
        let mut nodes = vec![method_node(
            "com.acme.Foo#pay/1",
            serde_json::json!([{ "name": "BankEndpoint", "attrs": { "value": "/pay" } }]),
        )];
        let mut edges = Vec::new();
        let rules = PatternRules { routes: vec![route_rule("BankEndpoint", Some("POST"))] };

        apply_pattern_rules(&mut nodes, &mut edges, &rules);

        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).expect("route");
        assert_eq!(route.id.as_str(), "Route:POST /pay");
        let props = route.props.as_ref().unwrap();
        assert_eq!(props["httpMethod"], "POST");
        assert_eq!(props["path"], "/pay");
        assert_eq!(props["source"], "custom_pattern");
        assert_eq!(props["handler"], "com.acme.Foo#pay/1");
        let hr = edges.iter().find(|e| e.kind == EdgeKind::HandlesRoute).unwrap();
        assert_eq!(hr.src.as_str(), "Method:com.acme.Foo#pay/1");
        assert_eq!(hr.dst.as_str(), "Route:POST /pay");
    }

    #[test]
    fn composes_class_prefix() {
        let mut nodes = vec![
            class_node(
                "com.acme.Foo",
                serde_json::json!([{ "name": "BankResource", "attrs": { "value": "/api" } }]),
            ),
            method_node(
                "com.acme.Foo#get/1",
                serde_json::json!([{ "name": "BankEndpoint", "attrs": { "value": "/users/{id}" } }]),
            ),
        ];
        let mut edges = Vec::new();
        let mut rule = route_rule("BankEndpoint", Some("GET"));
        rule.class_prefix_annotation = Some("BankResource".to_string());
        apply_pattern_rules(&mut nodes, &mut edges, &PatternRules { routes: vec![rule] });

        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(route.props.as_ref().unwrap()["path"], "/api/users/{id}");
    }

    #[test]
    fn method_from_attribute() {
        let mut nodes = vec![method_node(
            "com.acme.Foo#do/0",
            serde_json::json!([{ "name": "Endpoint", "attrs": { "value": "/x", "verb": "delete" } }]),
        )];
        let mut edges = Vec::new();
        let mut rule = route_rule("Endpoint", None);
        rule.method_attr = Some("verb".to_string());
        apply_pattern_rules(&mut nodes, &mut edges, &PatternRules { routes: vec![rule] });
        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(route.id.as_str(), "Route:DELETE /x");
    }

    #[test]
    fn pathless_annotation_routes_to_controller_root() {
        // Micronaut-style bare `@Get` on a `@Controller("/home")` → route at the controller base.
        let mut nodes = vec![
            class_node(
                "com.acme.Home",
                serde_json::json!([{ "name": "Controller", "attrs": { "value": "/home" } }]),
            ),
            method_node(
                "com.acme.Home#index/0",
                serde_json::json!([{ "name": "Get" }]),
            ),
        ];
        let mut edges = Vec::new();
        let mut rule = route_rule("Get", Some("GET"));
        rule.class_prefix_annotation = Some("Controller".to_string());
        apply_pattern_rules(&mut nodes, &mut edges, &PatternRules { routes: vec![rule] });
        let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
        assert_eq!(route.id.as_str(), "Route:GET /home");
    }

    #[test]
    fn no_rule_or_no_match_is_noop() {
        let mut nodes = vec![method_node(
            "com.acme.Foo#pay/1",
            serde_json::json!([{ "name": "Other", "attrs": { "value": "/pay" } }]),
        )];
        let mut edges = Vec::new();
        apply_pattern_rules(&mut nodes, &mut edges, &PatternRules { routes: vec![route_rule("BankEndpoint", Some("POST"))] });
        assert!(!nodes.iter().any(|n| n.kind == NodeKind::Route));
        assert!(edges.is_empty());
    }

    #[test]
    fn does_not_duplicate_existing_route() {
        let existing = Node {
            id: NodeId::new("Route:POST /pay"),
            kind: NodeKind::Route,
            name: "POST /pay".into(),
            qualified_name: Some("POST /pay".into()),
            file: "x".into(),
            range: Range::default(),
            props: None,
        };
        let mut nodes = vec![
            existing,
            method_node(
                "com.acme.Foo#pay/1",
                serde_json::json!([{ "name": "BankEndpoint", "attrs": { "value": "/pay" } }]),
            ),
        ];
        let mut edges = Vec::new();
        apply_pattern_rules(&mut nodes, &mut edges, &PatternRules { routes: vec![route_rule("BankEndpoint", Some("POST"))] });
        assert_eq!(nodes.iter().filter(|n| n.kind == NodeKind::Route).count(), 1);
    }
}
