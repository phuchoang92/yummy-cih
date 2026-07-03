use std::sync::Arc;

use cih_core::NodeId;
use cih_graph_store::{CommunityInfo, GraphStore};
use rmcp::{model::CallToolResult, ErrorData as McpError};

use crate::args::FeatureMapArgs;
use crate::search::SearchState;
use crate::utils::{json_result, to_mcp};

pub async fn feature_map(
    store: &Arc<dyn GraphStore>,
    search: &SearchState,
    args: FeatureMapArgs,
) -> Result<CallToolResult, McpError> {
    let limit = (if args.limit == 0 { 50 } else { args.limit }).clamp(1, 200);
    let hits = search
        .query_hits(&args.query, limit)
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

    let hit_ids: Vec<NodeId> = hits.iter().map(|h| h.node_id.clone()).collect();
    let memberships = store.symbol_communities(&hit_ids).await.map_err(to_mcp)?;

    let community_of: std::collections::BTreeMap<String, CommunityInfo> = memberships
        .into_iter()
        .map(|(nid, ci)| (nid.to_string(), ci))
        .collect();

    let mut clusters: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();
    for hit in &hits {
        let cluster_key = community_of
            .get(hit.node_id.as_str())
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "unclustered".to_string());
        clusters
            .entry(cluster_key)
            .or_default()
            .push(serde_json::json!({
                "id": hit.node_id.as_str(),
                "kind": hit.kind.label(),
                "name": hit.name,
                "file": hit.file,
                "score": hit.score,
            }));
    }

    let result: Vec<serde_json::Value> = clusters
        .into_iter()
        .map(|(name, symbols)| {
            serde_json::json!({
                "community": name,
                "symbol_count": symbols.len(),
                "symbols": symbols,
            })
        })
        .collect();

    json_result(&serde_json::json!({
        "query": args.query,
        "total_hits": hits.len(),
        "clusters": result,
    }))
}
