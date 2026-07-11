use cih_core::{ContractMatch, ContractMatchKind, EdgeKind, NodeKind, Registry};
use rmcp::{model::CallToolResult, ErrorData as McpError};

use crate::args::{ApiImpactArgs, GroupContractsArgs, ShapeCheckArgs};
use crate::utils::{
    json_result, load_artifact_edges, load_artifact_nodes, node_prop_str_owned,
    parse_contract_kind_filter, short_class_name, strip_response_wrapper,
};

/// Contract-sync freshness for a group: `(contracts_synced_at, contracts_stale)`.
/// Conservative on missing data: an unstamped or unregistered group reads as stale.
fn group_freshness(group_name: &str) -> (Option<String>, bool) {
    let state = cih_core::group_dir(group_name).and_then(|dir| cih_core::SyncState::load(&dir));
    let synced_at = state.as_ref().map(|s| s.synced_at.clone());
    let group_registry = cih_core::GroupRegistry::load();
    let Some(group) = group_registry.find(group_name) else {
        // Contracts were readable but the group is gone from groups.json —
        // freshness can't be verified against members, so flag it.
        return (synced_at, true);
    };
    let contracts_exist = cih_core::contracts_path(group_name).is_some_and(|path| path.exists());
    let stale =
        cih_core::group_contracts_stale(group, &Registry::load(), state.as_ref(), contracts_exist);
    (synced_at, stale)
}

pub async fn group_contracts(args: GroupContractsArgs) -> Result<CallToolResult, McpError> {
    let path = cih_core::contracts_path(&args.group).ok_or_else(|| {
        McpError::internal_error("cannot determine HOME for group contracts path", None)
    })?;
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        McpError::invalid_params(
            format!(
                "cannot read contracts for group '{}' at {}: {e}. Run `cih-engine group sync {}` first",
                args.group,
                path.display(),
                args.group
            ),
            None,
        )
    })?;
    let filter = parse_contract_kind_filter(if args.kind.is_empty() {
        None
    } else {
        Some(args.kind.as_str())
    })
    .map_err(|e| McpError::invalid_params(e, None))?;
    let mut matches = Vec::new();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let item: ContractMatch = serde_json::from_str(line).map_err(|e| {
            McpError::internal_error(format!("malformed contracts artifact: {e}"), None)
        })?;
        if filter.is_none() || filter == Some(item.kind) {
            matches.push(item);
        }
    }
    let (synced_at, stale) = group_freshness(&args.group);
    json_result(&serde_json::json!({
        "group": args.group,
        "contracts_synced_at": synced_at,
        "contracts_stale": stale,
        "matches": matches,
    }))
}

pub async fn api_impact(args: ApiImpactArgs) -> Result<CallToolResult, McpError> {
    let contracts_file = cih_core::contracts_path(&args.group).ok_or_else(|| {
        McpError::internal_error("cannot determine HOME for group contracts path", None)
    })?;
    let raw = std::fs::read_to_string(&contracts_file).map_err(|e| {
        McpError::invalid_params(
            format!(
                "cannot read contracts for group '{}': {e}. \
                 Run `cih-engine group sync {}` first",
                args.group, args.group
            ),
            None,
        )
    })?;
    let method = args.method.to_ascii_uppercase();
    let target_key = format!(
        "{} {}",
        method,
        cih_core::normalize_contract_path(&args.path)
    );
    let mut consumers = Vec::new();
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let item: ContractMatch = serde_json::from_str(line).map_err(|e| {
            McpError::internal_error(format!("malformed contracts artifact: {e}"), None)
        })?;
        if item.kind != ContractMatchKind::HttpRoute || item.match_key != target_key {
            continue;
        }
        consumers.push(serde_json::json!({
            "provider_repo": item.provider_repo,
            "provider_route": item.provider_id,
            "consumer_repo": item.consumer_repo,
            "consumer_endpoint": item.consumer_id,
        }));
    }
    let (synced_at, stale) = group_freshness(&args.group);
    json_result(&serde_json::json!({
        "method": method,
        "path": args.path,
        "match_key": target_key,
        "consumers": consumers,
        "contracts_synced_at": synced_at,
        "contracts_stale": stale,
    }))
}

pub async fn shape_check(args: ShapeCheckArgs) -> Result<CallToolResult, McpError> {
    let contracts_file = cih_core::contracts_path(&args.group).ok_or_else(|| {
        McpError::internal_error("cannot determine HOME for group contracts path", None)
    })?;
    let raw = std::fs::read_to_string(&contracts_file).map_err(|e| {
        McpError::invalid_params(
            format!(
                "cannot read contracts for group '{}': {e}. \
                 Run `cih-engine group sync {}` first",
                args.group, args.group
            ),
            None,
        )
    })?;
    let contracts: Vec<ContractMatch> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<ContractMatch>(l).ok())
        .filter(|c| {
            c.kind == ContractMatchKind::HttpRoute
                && c.provider_repo == args.provider
                && c.consumer_repo == args.consumer
        })
        .collect();
    if contracts.is_empty() {
        return json_result(&serde_json::json!({
            "provider": args.provider,
            "consumer": args.consumer,
            "contracts": [],
            "note": "No HTTP contracts found between these repos in the group."
        }));
    }

    let reg = Registry::load();
    let provider_entry = reg.find(&args.provider).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "provider '{}' not in registry; run analyze first",
                args.provider
            ),
            None,
        )
    })?;
    let consumer_entry = reg.find(&args.consumer).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "consumer '{}' not in registry; run analyze first",
                args.consumer
            ),
            None,
        )
    })?;

    let provider_nodes = load_artifact_nodes(&provider_entry.artifacts_dir)
        .map_err(|e| McpError::internal_error(format!("provider artifacts: {e}"), None))?;
    let consumer_nodes = load_artifact_nodes(&consumer_entry.artifacts_dir)
        .map_err(|e| McpError::internal_error(format!("consumer artifacts: {e}"), None))?;
    let consumer_edges = load_artifact_edges(&consumer_entry.artifacts_dir)
        .map_err(|e| McpError::internal_error(format!("consumer edges: {e}"), None))?;

    let provider_by_id: std::collections::BTreeMap<String, &cih_core::Node> = provider_nodes
        .iter()
        .map(|n| (n.id.as_str().to_string(), n))
        .collect();
    let consumer_by_id: std::collections::BTreeMap<String, &cih_core::Node> = consumer_nodes
        .iter()
        .map(|n| (n.id.as_str().to_string(), n))
        .collect();

    let mut ext_call_callers: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut method_accesses: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for edge in &consumer_edges {
        match edge.kind {
            EdgeKind::ExternalCall => {
                ext_call_callers
                    .entry(edge.dst.as_str().to_string())
                    .or_default()
                    .push(edge.src.as_str().to_string());
            }
            EdgeKind::Accesses => {
                method_accesses
                    .entry(edge.src.as_str().to_string())
                    .or_default()
                    .push(edge.dst.as_str().to_string());
            }
            _ => {}
        }
    }

    let mut results = Vec::new();
    for contract in &contracts {
        let route_node = provider_by_id.get(&contract.provider_id);
        let handler_sig = route_node.and_then(|n| node_prop_str_owned(n, "handler"));
        let method_node = handler_sig
            .as_ref()
            .and_then(|sig| provider_by_id.get(&format!("Method:{sig}")));
        let return_type_raw = method_node.and_then(|n| node_prop_str_owned(n, "returnType"));
        let dto_short = return_type_raw
            .as_deref()
            .map(strip_response_wrapper)
            .unwrap_or("");

        let provider_fields: Vec<String> = if dto_short.is_empty() {
            vec![]
        } else {
            let dto_fqcns: Vec<String> = provider_nodes
                .iter()
                .filter(|n| matches!(n.kind, NodeKind::Class | NodeKind::Record))
                .filter(|n| short_class_name(&n.name) == dto_short)
                .filter_map(|n| n.qualified_name.clone().or_else(|| Some(n.name.clone())))
                .collect();
            provider_nodes
                .iter()
                .filter(|n| n.kind == NodeKind::Field)
                .filter(|n| {
                    n.qualified_name
                        .as_deref()
                        .map(|qn| {
                            dto_fqcns
                                .iter()
                                .any(|fqcn| qn.starts_with(&format!("{fqcn}#")))
                        })
                        .unwrap_or(false)
                })
                .map(|n| n.name.clone())
                .collect()
        };

        let caller_ids: Vec<String> = ext_call_callers
            .get(&contract.consumer_id)
            .cloned()
            .unwrap_or_default();
        let mut consumer_accessed: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for caller_id in &caller_ids {
            if let Some(field_ids) = method_accesses.get(caller_id) {
                for fid in field_ids {
                    if let Some(fn_node) = consumer_by_id.get(fid) {
                        consumer_accessed.insert(fn_node.name.clone());
                    }
                }
            }
        }

        let provider_set: std::collections::BTreeSet<String> =
            provider_fields.iter().cloned().collect();
        let provider_only: Vec<String> = provider_fields
            .iter()
            .filter(|f| !consumer_accessed.contains(*f))
            .cloned()
            .collect();
        let consumer_only: Vec<String> = consumer_accessed
            .iter()
            .filter(|f| !provider_set.contains(*f))
            .cloned()
            .collect();
        let matched: Vec<String> = provider_fields
            .iter()
            .filter(|f| consumer_accessed.contains(*f))
            .cloned()
            .collect();

        results.push(serde_json::json!({
            "provider_route": contract.provider_id,
            "consumer_endpoint": contract.consumer_id,
            "provider_handler": handler_sig,
            "provider_return_type": return_type_raw,
            "provider_dto": if dto_short.is_empty() { None } else { Some(dto_short) },
            "provider_fields": provider_fields,
            "consumer_accessed_fields": consumer_accessed.into_iter().collect::<Vec<_>>(),
            "matched": matched,
            "provider_only": provider_only,
            "consumer_only": consumer_only,
            "note": if return_type_raw.is_none() {
                Some("returnType not available — re-run `cih-engine analyze` to populate it")
            } else {
                None
            },
        }));
    }

    let (synced_at, stale) = group_freshness(&args.group);
    json_result(&serde_json::json!({
        "provider": args.provider,
        "consumer": args.consumer,
        "contracts": results,
        "contracts_synced_at": synced_at,
        "contracts_stale": stale,
    }))
}
