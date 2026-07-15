//! Cypher (de)serialization helpers — convert `Node`/`Edge` to the `UNWIND` list
//! literals the loader inlines, escape scalars for the `CYPHER` param preamble,
//! and read scalar cells back out of a `GRAPH.QUERY` reply.

use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};
use redis::Value;

pub(super) fn node_from_row(r: &[String]) -> Node {
    Node {
        id: NodeId::new(r.first().cloned().unwrap_or_default()),
        kind: NodeKind::from_label(r.get(1).map(String::as_str).unwrap_or("")),
        name: r.get(2).cloned().unwrap_or_default(),
        qualified_name: r.get(3).filter(|s| !s.is_empty()).cloned(),
        file: r.get(4).cloned().unwrap_or_default(),
        range: Range::default(),
        props: None,
    }
}

pub(super) fn nodes_to_list(nodes: &[Node]) -> String {
    let items: Vec<String> = nodes
        .iter()
        .map(|n| {
            let props_json = n.props.as_ref().map(serde_json::Value::to_string);
            let id = cstr(n.id.as_str());
            let name = cstr(&n.name);
            let kind = cstr(n.kind.label());
            let file = cstr(&n.file);
            let qn = copt(n.qualified_name.as_deref());
            let sl = n.range.start_line;
            let el = n.range.end_line;
            let props = copt(props_json.as_deref());
            let stereotype = copt(prop_str(n, "stereotype"));
            let http_method = copt(prop_str(n, "httpMethod"));
            let path = copt(prop_str(n, "path"));
            let decorator = copt(prop_str(n, "decorator"));
            let handler = copt(prop_str(n, "handler"));
            let symbol_count = cnum_u64(prop_u64(n, "symbolCount").or_else(|| prop_u64(n, "symbol_count")));
            let cohesion = cnum_f64(prop_f64(n, "cohesion"));
            let process_type = copt(prop_str(n, "process_type"));
            // Gap 1: promoted complexity fields (queryable as first-class graph properties)
            let cyclomatic = cnum_u64(prop_u64(n, "cyclomatic"));
            let cognitive = cnum_u64(prop_u64(n, "cognitive"));
            let loop_depth = cnum_u64(prop_u64(n, "loopDepth"));
            let transitive_ld = cnum_u64(prop_u64(n, "transitiveLoopDepth"));
            format!(
                "{{id:{id}, name:{name}, kind:{kind}, file:{file}, qn:{qn}, sl:{sl}, el:{el}, props:{props}, stereotype:{stereotype}, httpMethod:{http_method}, path:{path}, decorator:{decorator}, handler:{handler}, symbolCount:{symbol_count}, cohesion:{cohesion}, processType:{process_type}, cyclomatic:{cyclomatic}, cognitive:{cognitive}, loopDepth:{loop_depth}, transitiveLoopDepth:{transitive_ld}}}"
            )
        })
        .collect();
    format!("[{}]", items.join(", "))
}

pub(super) fn prop_str<'a>(node: &'a Node, key: &str) -> Option<&'a str> {
    node.props.as_ref()?.get(key)?.as_str()
}

pub(super) fn prop_u64(node: &Node, key: &str) -> Option<u64> {
    node.props.as_ref()?.get(key)?.as_u64()
}

pub(super) fn prop_f64(node: &Node, key: &str) -> Option<f64> {
    node.props.as_ref()?.get(key)?.as_f64()
}

pub(super) fn cnum_u64(v: Option<u64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "null".into())
}

pub(super) fn cnum_f64(v: Option<f64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "null".into())
}

pub(super) fn edges_to_list(edges: &[&Edge]) -> String {
    let items: Vec<String> = edges
        .iter()
        .map(|e| {
            // Gap 3: serialize call_sites array from props as a JSON string column.
            let call_sites = e
                .props
                .as_ref()
                .and_then(|p| p.get("call_sites"))
                .map(|v| v.to_string());
            let cs = copt(call_sites.as_deref());
            format!(
                "{{src:{}, dst:{}, conf:{}, reason:{}, callSites:{}}}",
                cstr(e.src.as_str()),
                cstr(e.dst.as_str()),
                e.confidence,
                cstr(&e.reason),
                cs,
            )
        })
        .collect();
    format!("[{}]", items.join(", "))
}

pub(super) fn rel_filter(kinds: &[EdgeKind]) -> String {
    if kinds.is_empty() {
        String::new()
    } else {
        let labels: Vec<&str> = kinds.iter().map(|k| k.cypher_label()).collect();
        format!(":{}", labels.join("|"))
    }
}

pub(super) fn edge_from_label(label: &str) -> EdgeKind {
    match label {
        "CONTAINS" => EdgeKind::Contains,
        "CALLS" => EdgeKind::Calls,
        "EXTENDS" => EdgeKind::Extends,
        "IMPLEMENTS" => EdgeKind::Implements,
        "HAS_METHOD" => EdgeKind::HasMethod,
        "HAS_FIELD" => EdgeKind::HasField,
        "IMPORTS" => EdgeKind::Imports,
        "ACCESSES" => EdgeKind::Accesses,
        "USES" => EdgeKind::Uses,
        "METHOD_OVERRIDES" => EdgeKind::MethodOverrides,
        "METHOD_IMPLEMENTS" => EdgeKind::MethodImplements,
        "MEMBER_OF" => EdgeKind::MemberOf,
        "STEP_IN_PROCESS" => EdgeKind::StepInProcess,
        "HANDLES_ROUTE" => EdgeKind::HandlesRoute,
        "PUBLISHES_EVENT" => EdgeKind::PublishesEvent,
        "LISTENS_TO" => EdgeKind::ListensTo,
        "EXTERNAL_CALL" => EdgeKind::ExternalCall,
        "TESTS" => EdgeKind::Tests,
        "SIMILAR_TO" => EdgeKind::SimilarTo,
        _ => EdgeKind::Other,
    }
}

/// Cypher string literal with escaping (`'...'`). Used both in the `CYPHER`
/// parameter preamble and inside generated UNWIND list literals.
pub(super) fn cstr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

/// Optional Cypher string literal → `'...'` or `null`.
pub(super) fn copt(s: Option<&str>) -> String {
    match s {
        Some(v) => cstr(v),
        None => "null".to_string(),
    }
}

pub(super) fn as_array(v: &Value) -> Vec<&Value> {
    match v {
        Value::Array(items) => items.iter().collect(),
        _ => vec![],
    }
}

pub(super) fn cell_to_string(v: &&Value) -> String {
    match v {
        Value::Nil => String::new(),
        Value::Int(i) => i.to_string(),
        Value::BulkString(b) => String::from_utf8_lossy(b).into_owned(),
        Value::SimpleString(s) => s.clone(),
        Value::Double(d) => d.to_string(),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(|x| cell_to_string(&x)).collect();
            format!("[{}]", inner.join(", "))
        }
        other => format!("{other:?}"),
    }
}
