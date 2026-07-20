use cih_core::{ContractMatchKind, Edge, GraphArtifacts, Node, VersionId};
use cih_graph_store::{Direction, GraphStoreError};
use rmcp::{
    model::{CallToolResult, Content},
    ErrorData as McpError,
};

pub fn to_mcp(e: GraphStoreError) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

/// Standard "unknown repo" error: the client named a `repo` that is not in the
/// registry. This is client input, so it maps to `invalid_params` (not
/// `internal_error`) — use this everywhere to keep the failure code consistent.
pub fn repo_not_found(name: &str) -> McpError {
    McpError::invalid_params(format!("repo '{name}' not in registry"), None)
}

pub fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let content =
        Content::json(value).map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![content]))
}

pub fn text_result(s: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(s)]))
}

impl From<crate::args::DirectionArg> for Direction {
    fn from(d: crate::args::DirectionArg) -> Self {
        match d {
            crate::args::DirectionArg::Upstream => Direction::Upstream,
            crate::args::DirectionArg::Downstream => Direction::Downstream,
            crate::args::DirectionArg::Both => Direction::Both,
        }
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

/// Resolve `repo` (name/path, or empty for the active graph key) to its full
/// registry entry.
pub fn resolve_repo_entry(repo: &str, graph_key: &str) -> Result<cih_core::RegistryEntry, String> {
    let reg = cih_core::Registry::load_cached();
    if reg.entries.is_empty() {
        return Err("no repos in registry — run `cih-engine analyze <repo>` first".to_string());
    }
    let entry = if repo.is_empty() {
        reg.entries
            .iter()
            .find(|e| e.graph_key == graph_key)
            .ok_or_else(|| {
                format!("no repo registered for graph_key '{graph_key}'; pass `repo` explicitly")
            })?
    } else {
        reg.find(repo)
            .ok_or_else(|| format!("repo '{repo}' not in registry"))?
    };
    Ok(entry.clone())
}

/// Resolve `repo` (name/path, or empty for the active graph key) to
/// `(repo_path, artifacts_dir)` via the registry.
pub fn resolve_repo(repo: &str, graph_key: &str) -> Result<(String, String), String> {
    resolve_repo_entry(repo, graph_key).map(|entry| (entry.path, entry.artifacts_dir))
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
