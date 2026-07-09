use cih_core::{ContractMatchKind, Edge, GraphArtifacts, Node, VersionId};
use cih_graph_store::{Direction, GraphStoreError};
use rmcp::{
    model::{CallToolResult, Content},
    ErrorData as McpError,
};

pub fn to_mcp(e: GraphStoreError) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

pub fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let content =
        Content::json(value).map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![content]))
}

pub fn text_result(s: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(s)]))
}

pub fn parse_direction(direction: Option<&str>) -> Direction {
    match direction {
        Some("downstream") => Direction::Downstream,
        Some("both") => Direction::Both,
        _ => Direction::Upstream,
    }
}

pub fn load_artifact_nodes(artifacts_dir: &str) -> std::io::Result<Vec<Node>> {
    let dir = std::path::Path::new(artifacts_dir);
    GraphArtifacts {
        nodes_path: dir.join("nodes.jsonl"),
        edges_path: dir.join("edges.jsonl"),
        version: VersionId::new(String::new()),
    }
    .read_nodes()
}

pub fn load_artifact_edges(artifacts_dir: &str) -> std::io::Result<Vec<Edge>> {
    let dir = std::path::Path::new(artifacts_dir);
    GraphArtifacts {
        nodes_path: dir.join("nodes.jsonl"),
        edges_path: dir.join("edges.jsonl"),
        version: VersionId::new(String::new()),
    }
    .read_edges()
}

pub fn node_prop_str_owned(node: &Node, key: &str) -> Option<String> {
    node.props.as_ref()?.get(key)?.as_str().map(str::to_owned)
}

pub fn strip_response_wrapper(raw: &str) -> &str {
    raw.find('<')
        .and_then(|i| raw.rfind('>').map(|j| &raw[i + 1..j]))
        .unwrap_or(raw)
}

pub fn short_class_name(fqcn: &str) -> &str {
    fqcn.rsplit('.').next().unwrap_or(fqcn)
}

pub fn parse_contract_kind_filter(
    kind: Option<&str>,
) -> std::result::Result<Option<ContractMatchKind>, String> {
    match kind.unwrap_or("all").trim().to_ascii_lowercase().as_str() {
        "" | "all" => Ok(None),
        "http" | "http_route" | "http-route" => Ok(Some(ContractMatchKind::HttpRoute)),
        "kafka" | "kafka_topic" | "kafka-topic" => Ok(Some(ContractMatchKind::KafkaTopic)),
        "spring" | "spring_event" | "spring-event" => Ok(Some(ContractMatchKind::SpringEvent)),
        other => Err(format!(
            "unknown contract kind '{other}'; expected all, http, kafka, or spring"
        )),
    }
}
