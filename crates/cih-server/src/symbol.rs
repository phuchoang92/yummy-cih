use std::sync::Arc;

use cih_core::{Node, NodeId};
use cih_graph_store::GraphStore;
use rmcp::ErrorData as McpError;
use serde::Serialize;

use crate::utils::to_mcp;

pub enum SymbolResolution {
    Id(NodeId),
    Ambiguous(Vec<Node>),
    NotFound,
}

#[derive(Serialize)]
pub struct AmbiguousCandidate {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub file: String,
}

#[derive(Serialize)]
pub struct AmbiguousResult {
    pub status: &'static str,
    pub candidates: Vec<AmbiguousCandidate>,
}

impl AmbiguousResult {
    pub fn from_nodes(nodes: Vec<Node>) -> Self {
        AmbiguousResult {
            status: "ambiguous",
            candidates: nodes
                .into_iter()
                .map(|n| AmbiguousCandidate {
                    id: n.id.to_string(),
                    kind: n.kind.label().to_string(),
                    name: n.name,
                    file: n.file,
                })
                .collect(),
        }
    }
}

/// Resolve a name to a NodeId: if it already contains `:` treat it as a
/// full NodeId; otherwise query for candidates and disambiguate.
pub async fn resolve_symbol(
    store: &Arc<dyn GraphStore>,
    name: &str,
) -> Result<SymbolResolution, McpError> {
    if name.contains(':') {
        return Ok(SymbolResolution::Id(NodeId::new(name.to_string())));
    }
    let candidates = store.candidates_by_name(name, 10).await.map_err(to_mcp)?;
    Ok(match candidates.len() {
        0 => SymbolResolution::NotFound,
        1 => SymbolResolution::Id(candidates.into_iter().next().unwrap().id),
        _ => SymbolResolution::Ambiguous(candidates),
    })
}
