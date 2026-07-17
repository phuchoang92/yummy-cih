use std::sync::Arc;

use cih_graph_store::{Direction, GraphStore};
use rmcp::{model::CallToolResult, ErrorData as McpError};
use serde::Serialize;

use crate::args::DetectChangesArgs;
use crate::symbol::{find_repo_path, git_changed_files};
use crate::utils::{json_result, to_mcp};

pub async fn detect_changes(
    store: &Arc<dyn GraphStore>,
    graph_key: &str,
    args: DetectChangesArgs,
) -> Result<CallToolResult, McpError> {
    let repo_path = find_repo_path(
        if args.repo.is_empty() {
            None
        } else {
            Some(args.repo.as_str())
        },
        graph_key,
    )
    .map_err(|e| McpError::invalid_params(e, None))?;

    let changed_files = git_changed_files(
        &repo_path,
        &args.scope,
        if args.base_ref.is_empty() {
            None
        } else {
            Some(args.base_ref.as_str())
        },
    )
    .map_err(|e| McpError::internal_error(e, None))?;

    if changed_files.is_empty() {
        #[derive(Serialize)]
        struct Empty {
            changed_files: Vec<String>,
            changed_symbols: Vec<serde_json::Value>,
            affected_symbols: Vec<String>,
            affected_processes: Vec<String>,
            risk: &'static str,
        }
        return json_result(&Empty {
            changed_files,
            changed_symbols: vec![],
            affected_symbols: vec![],
            affected_processes: vec![],
            risk: "none",
        });
    }

    let changed_nodes = store.nodes_in_files(&changed_files).await.map_err(to_mcp)?;

    // Fan the per-node blast-radius traversals out concurrently instead of
    // awaiting up to 20 in series; the FalkorStore query_limit semaphore
    // backpressures, and results merge into a set so completion order is moot.
    let symbol_limit = changed_nodes.len().min(20);
    let mut set = tokio::task::JoinSet::new();
    for node in &changed_nodes[..symbol_limit] {
        let store = store.clone();
        let id = node.id.clone();
        set.spawn(async move { store.impact(&id, Direction::Upstream, 4).await });
    }
    let mut affected_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    while let Some(joined) = set.join_next().await {
        if let Ok(Ok(impact)) = joined {
            for n in &impact.affected {
                affected_set.insert(n.id.to_string());
            }
        }
    }
    for node in &changed_nodes {
        affected_set.remove(node.id.as_str());
    }
    let mut affected_symbols: Vec<String> = affected_set.into_iter().collect();
    affected_symbols.sort();

    let changed_ids: Vec<cih_core::NodeId> = changed_nodes.iter().map(|n| n.id.clone()).collect();
    let affected_processes = store
        .processes_for_symbols(&changed_ids)
        .await
        .map_err(to_mcp)?;

    let risk = cih_graph_store::risk_from_fanout(affected_symbols.len());

    #[derive(Serialize)]
    struct ChangedSymbol {
        id: String,
        kind: String,
        name: String,
        file: String,
    }
    let changed_symbols: Vec<ChangedSymbol> = changed_nodes
        .iter()
        .map(|n| ChangedSymbol {
            id: n.id.to_string(),
            kind: n.kind.label().to_string(),
            name: n.name.clone(),
            file: n.file.clone(),
        })
        .collect();

    #[derive(Serialize)]
    struct Out {
        changed_files: Vec<String>,
        changed_symbols: Vec<ChangedSymbol>,
        affected_symbols: Vec<String>,
        affected_processes: Vec<String>,
        risk: &'static str,
    }
    json_result(&Out {
        changed_files,
        changed_symbols,
        affected_symbols,
        affected_processes,
        risk,
    })
}
