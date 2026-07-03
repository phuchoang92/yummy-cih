use std::sync::Arc;

use cih_graph_store::GraphStore;
use rmcp::{model::CallToolResult, ErrorData as McpError};

use crate::args::{RegressionScopeArgs, TestCoverageArgs, UntestedPathsArgs};
use crate::symbol::{AmbiguousCandidate, AmbiguousResult, SymbolResolution, resolve_symbol};
use crate::utils::{json_result, to_mcp};

pub async fn test_coverage(
    store: &Arc<dyn GraphStore>,
    args: TestCoverageArgs,
) -> Result<CallToolResult, McpError> {
    let id = match resolve_symbol(store, &args.name).await? {
        SymbolResolution::Id(id) => id,
        SymbolResolution::Ambiguous(candidates) => {
            return json_result(&AmbiguousResult {
                status: "ambiguous",
                candidates: candidates
                    .iter()
                    .map(|n| AmbiguousCandidate {
                        id: n.id.to_string(),
                        kind: n.kind.label().to_string(),
                        name: n.name.clone(),
                        file: n.file.clone(),
                    })
                    .collect(),
            });
        }
        SymbolResolution::NotFound => {
            return Err(McpError::invalid_params(
                format!("symbol '{}' not found", args.name),
                None,
            ));
        }
    };
    let tests = store.test_coverage(&id).await.map_err(to_mcp)?;
    json_result(&serde_json::json!({
        "symbol_id": id.as_str(),
        "test_count": tests.len(),
        "tests": tests.iter().map(|n| serde_json::json!({
            "id": n.id.as_str(),
            "kind": n.kind.label(),
            "name": n.name,
            "file": n.file,
        })).collect::<Vec<_>>(),
    }))
}

pub async fn regression_scope(
    store: &Arc<dyn GraphStore>,
    args: RegressionScopeArgs,
) -> Result<CallToolResult, McpError> {
    let tests = store
        .tests_for_files(&args.changed_files)
        .await
        .map_err(to_mcp)?;
    let mut seen_files = std::collections::BTreeSet::new();
    let test_classes: Vec<serde_json::Value> = tests
        .iter()
        .filter(|n| seen_files.insert(n.file.clone()))
        .map(|n| {
            serde_json::json!({
                "id": n.id.as_str(),
                "kind": n.kind.label(),
                "name": n.name,
                "file": n.file,
            })
        })
        .collect();
    json_result(&serde_json::json!({
        "changed_file_count": args.changed_files.len(),
        "test_class_count": test_classes.len(),
        "test_classes": test_classes,
    }))
}

pub async fn untested_paths(
    store: &Arc<dyn GraphStore>,
    args: UntestedPathsArgs,
) -> Result<CallToolResult, McpError> {
    let limit = (if args.limit == 0 { 50 } else { args.limit }).clamp(1, 500);
    let symbols = store
        .untested_symbols(&args.module_prefix, limit)
        .await
        .map_err(to_mcp)?;
    json_result(&serde_json::json!({
        "prefix": args.module_prefix,
        "untested_count": symbols.len(),
        "symbols": symbols.iter().map(|n| serde_json::json!({
            "id": n.id.as_str(),
            "kind": n.kind.label(),
            "name": n.name,
            "file": n.file,
        })).collect::<Vec<_>>(),
    }))
}
