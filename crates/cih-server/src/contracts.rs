use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use cih_core::{ContractMatch, ContractMatchKind, EdgeKind, NodeKind};
use rmcp::{model::CallToolResult, ErrorData as McpError};

use crate::args::{ApiImpactArgs, GroupContractsArgs, ShapeCheckArgs, TraceFlowXArgs};
use crate::artifact_cache::{ArtifactRepository, ArtifactSnapshot};
use crate::blocking::{blocking_timeout, run_blocking_heavy};
use crate::repo_context::{RepoCatalogSnapshot, RepoContextProvider, RepoSelector};
use crate::utils::{
    app_error_to_mcp, json_result, node_prop_str_owned, parse_contract_kind_filter,
    short_class_name, strip_response_wrapper,
};
use crate::xflow::{self, XflowState};

/// Read and parse a group's synced contracts, with the canonical
/// "run group sync first" error when they don't exist yet.
fn load_group_contracts(group: &str) -> Result<Vec<ContractMatch>, McpError> {
    let path = cih_core::contracts_path(group).ok_or_else(|| {
        McpError::internal_error("cannot determine HOME for group contracts path", None)
    })?;
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        McpError::invalid_params(
            format!(
                "cannot read contracts for group '{group}': {e}. \
                 Run `cih-engine group sync {group}` first"
            ),
            None,
        )
    })?;
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<ContractMatch>(line).map_err(|e| {
                McpError::internal_error(format!("malformed contracts artifact: {e}"), None)
            })
        })
        .collect()
}

/// Contract-sync freshness for a group: `(contracts_synced_at, contracts_stale)`.
/// Conservative on missing data: an unstamped or unregistered group reads as stale.
fn group_freshness(group_name: &str, catalog: &RepoCatalogSnapshot) -> (Option<String>, bool) {
    let state = cih_core::group_dir(group_name).and_then(|dir| cih_core::SyncState::load(&dir));
    let synced_at = state.as_ref().map(|s| s.synced_at.clone());
    let Some(group) = catalog.groups().find(group_name) else {
        // Contracts were readable but the group is gone from groups.json —
        // freshness can't be verified against members, so flag it.
        return (synced_at, true);
    };
    let contracts_exist = cih_core::contracts_path(group_name).is_some_and(|path| path.exists());
    let stale =
        cih_core::group_contracts_stale(group, catalog.registry(), state.as_ref(), contracts_exist);
    (synced_at, stale)
}

/// The handlers below are thin async shims: each body is synchronous cold I/O
/// (contracts file reads, artifact graph loads) plus pure compute, so one
/// `run_blocking` closure owns the whole phase — a cold multi-thousand-node
/// artifact parse must never run on a Tokio worker.
pub async fn group_contracts(
    args: GroupContractsArgs,
    repo_contexts: Arc<dyn RepoContextProvider>,
) -> Result<CallToolResult, McpError> {
    run_blocking_heavy(blocking_timeout(), "group_contracts load", move || {
        let catalog = repo_contexts.catalog_snapshot();
        group_contracts_sync(args, &catalog)
    })
    .await?
}

fn group_contracts_sync(
    args: GroupContractsArgs,
    catalog: &RepoCatalogSnapshot,
) -> Result<CallToolResult, McpError> {
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
    let (synced_at, stale) = group_freshness(&args.group, catalog);
    json_result(&serde_json::json!({
        "group": args.group,
        "contracts_synced_at": synced_at,
        "contracts_stale": stale,
        "matches": matches,
    }))
}

pub async fn api_impact(
    args: ApiImpactArgs,
    repo_contexts: Arc<dyn RepoContextProvider>,
    xflow: &XflowState,
) -> Result<CallToolResult, McpError> {
    let group = args.group.clone();
    let (catalog, contracts) =
        run_blocking_heavy(blocking_timeout(), "api_impact contract load", move || {
            let catalog = repo_contexts.catalog_snapshot();
            let contracts = load_group_contracts(&group)?;
            Ok::<_, McpError>((catalog, contracts))
        })
        .await??;
    let method = args.method.to_ascii_uppercase();
    let target_key = format!(
        "{} {}",
        method,
        cih_core::normalize_contract_path(&args.path)
    );
    let mut graphs = HashMap::new();
    if args.include_callers {
        for consumer in contracts
            .iter()
            .filter(|item| {
                item.kind == ContractMatchKind::HttpRoute && item.match_key == target_key
            })
            .map(|item| item.consumer_repo.clone())
            .collect::<HashSet<_>>()
        {
            let loaded = match catalog.resolve(RepoSelector::NameOrPath(consumer.clone())) {
                Ok(repo) => xflow
                    .graph_for(&repo)
                    .await
                    .map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            graphs.insert(consumer, loaded);
        }
    }
    run_blocking_heavy(blocking_timeout(), "api_impact analysis", move || {
        api_impact_sync(args, &catalog, &contracts, &graphs)
    })
    .await?
}

fn api_impact_sync(
    args: ApiImpactArgs,
    catalog: &RepoCatalogSnapshot,
    contracts: &[ContractMatch],
    graphs: &HashMap<String, Result<Arc<xflow::ArtifactGraph>, String>>,
) -> Result<CallToolResult, McpError> {
    let method = args.method.to_ascii_uppercase();
    let target_key = format!(
        "{} {}",
        method,
        cih_core::normalize_contract_path(&args.path)
    );
    let caller_depth = (if args.caller_depth == 0 {
        3
    } else {
        args.caller_depth
    })
    .clamp(1, 6);
    let mut consumers = Vec::new();
    for item in contracts {
        if item.kind != ContractMatchKind::HttpRoute || item.match_key != target_key {
            continue;
        }
        let mut row = serde_json::json!({
            "provider_repo": item.provider_repo,
            "provider_route": item.provider_id,
            "consumer_repo": item.consumer_repo,
            "consumer_endpoint": item.consumer_id,
        });
        if args.include_callers {
            row["consumer_callers"] = match consumer_callers(
                graphs.get(&item.consumer_repo),
                &item.consumer_repo,
                &item.consumer_id,
                caller_depth,
            ) {
                Ok(callers) => serde_json::json!(callers),
                Err(reason) => serde_json::json!({ "error": reason }),
            };
        }
        consumers.push(row);
    }
    let (synced_at, stale) = group_freshness(&args.group, catalog);
    json_result(&serde_json::json!({
        "method": method,
        "path": args.path,
        "match_key": target_key,
        "consumers": consumers,
        "contracts_synced_at": synced_at,
        "contracts_stale": stale,
    }))
}

#[derive(serde::Serialize)]
struct ConsumerCaller {
    method_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    route: Option<String>,
}

/// Reverse-CALLS walk in the consumer repo: methods that (transitively) reach
/// the `ExternalCall` site, each with its enclosing route when one handles it.
fn consumer_callers(
    graph: Option<&Result<Arc<xflow::ArtifactGraph>, String>>,
    consumer_repo: &str,
    consumer_endpoint: &str,
    depth_limit: u32,
) -> Result<Vec<ConsumerCaller>, String> {
    let graph = graph
        .ok_or_else(|| format!("consumer '{consumer_repo}' graph was not loaded"))?
        .as_ref()
        .map_err(|error| format!("consumer artifacts unreadable: {error}"))?;

    // Direct callers: ExternalCall edges into the endpoint node.
    let mut queue: VecDeque<(String, u32)> = graph
        .incoming(consumer_endpoint)
        .filter(|edge| edge.kind == EdgeKind::ExternalCall)
        .map(|edge| (edge.src.as_str().to_string(), 0))
        .collect();
    let mut seen: HashSet<String> = queue.iter().map(|(id, _)| id.clone()).collect();
    let mut callers = Vec::new();

    while let Some((method_id, depth)) = queue.pop_front() {
        let route = graph
            .out(&method_id)
            .find(|edge| edge.kind == EdgeKind::HandlesRoute)
            .map(|edge| edge.dst.as_str().to_string());
        callers.push(ConsumerCaller {
            method_id: method_id.clone(),
            route,
        });
        if depth >= depth_limit {
            continue;
        }
        for edge in graph.incoming(&method_id) {
            if edge.kind != EdgeKind::Calls {
                continue;
            }
            let src = edge.src.as_str().to_string();
            if seen.insert(src.clone()) {
                queue.push_back((src, depth + 1));
            }
        }
    }
    Ok(callers)
}

/// `Err(message naming the group's members)` when `repo_name` is not one of them.
pub(crate) fn validate_group_member(
    group: &str,
    members: &[String],
    repo_name: &str,
) -> Result<(), String> {
    if members.iter().any(|member| member == repo_name) {
        return Ok(());
    }
    Err(format!(
        "repo '{repo_name}' is not a member of group '{group}' (members: {}) — \
         pass `repo` naming one of them or add it with `cih-engine group add`",
        members.join(", ")
    ))
}

/// Cross-repo downstream trace: walk the start repo's artifacts, hop through
/// the group's contract matches into provider/consumer repos, continue there.
/// The start repo is `args.repo` (registry name/path) or, when empty, the
/// first registry entry bound to the server's graph key.
pub async fn trace_flow_x(
    args: TraceFlowXArgs,
    repo_contexts: Arc<dyn RepoContextProvider>,
    xflow: &XflowState,
) -> Result<CallToolResult, McpError> {
    let group = args.group.clone();
    let (catalog, contracts) = run_blocking_heavy(
        blocking_timeout(),
        "trace_flow_x contract load",
        move || {
            let catalog = repo_contexts.catalog_snapshot();
            let contracts = load_group_contracts(&group)?;
            Ok::<_, McpError>((catalog, contracts))
        },
    )
    .await??;
    let repo = catalog
        .resolve(RepoSelector::from_wire(&args.repo))
        .map_err(app_error_to_mcp)?;
    let start_repo = repo.registry_entry.name.clone();

    let group_entry = catalog.groups().find(&args.group).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "group '{}' not found — create it with `cih-engine group create` and sync it",
                args.group
            ),
            None,
        )
    })?;
    validate_group_member(&args.group, &group_entry.repos, &start_repo)
        .map_err(|e| McpError::invalid_params(e, None))?;
    let group_members = group_entry.repos.clone();

    let start_graph = xflow
        .graph_for(&repo)
        .await
        .map_err(|e| {
            McpError::invalid_params(
                format!(
                    "cannot load artifacts for '{start_repo}': {e} — re-run `cih-engine analyze {start_repo}`"
                ),
                None,
            )
        })?;

    let entry_id = match xflow::resolve_entry(&start_graph, &args.entry_point) {
        Ok(id) => id,
        Err(candidates) if candidates.is_empty() => {
            return Err(McpError::invalid_params(
                format!(
                    "entry point '{}' not found in repo '{start_repo}'",
                    args.entry_point
                ),
                None,
            ));
        }
        Err(candidates) => {
            return json_result(&serde_json::json!({
                "status": "ambiguous",
                "candidates": candidates,
            }));
        }
    };

    let max_depth = (if args.max_depth == 0 {
        xflow::DEFAULT_DEPTH
    } else {
        args.max_depth
    })
    .clamp(1, xflow::MAX_DEPTH);
    let max_hops = (if args.max_hops == 0 {
        xflow::DEFAULT_HOPS
    } else {
        args.max_hops
    })
    .clamp(1, 5);

    let mut graphs = HashMap::from([(start_repo.clone(), start_graph)]);
    for name in &group_members {
        if graphs.contains_key(name) {
            continue;
        }
        let Ok(repo) = catalog.resolve(RepoSelector::NameOrPath(name.clone())) else {
            continue;
        };
        if let Ok(graph) = xflow.graph_for(&repo).await {
            graphs.insert(repo.registry_entry.name, graph);
        }
    }
    run_blocking_heavy(blocking_timeout(), "trace_flow_x analysis", move || {
        let mut source = |repo: &str| graphs.get(repo).cloned();
        let trace = xflow::trace_across(
            &mut source,
            &contracts,
            &start_repo,
            &entry_id,
            max_depth,
            max_hops,
        );
        let (synced_at, stale) = group_freshness(&args.group, &catalog);
        json_result(&serde_json::json!({
            "entry_point": entry_id,
            "repo": start_repo,
            "group": args.group,
            "max_depth": max_depth,
            "max_hops": max_hops,
            "contracts_synced_at": synced_at,
            "contracts_stale": stale,
            "step_count": trace.steps.len(),
            "steps": trace.steps,
            "truncated": trace.truncated,
        }))
    })
    .await?
}

pub async fn shape_check(
    args: ShapeCheckArgs,
    repo_contexts: Arc<dyn RepoContextProvider>,
    artifacts: &Arc<dyn ArtifactRepository>,
) -> Result<CallToolResult, McpError> {
    let group = args.group.clone();
    let provider_name = args.provider.clone();
    let consumer_name = args.consumer.clone();
    let (catalog, contracts) =
        run_blocking_heavy(blocking_timeout(), "shape_check contract load", move || {
            let catalog = repo_contexts.catalog_snapshot();
            let contracts = load_group_contracts(&group)?
                .into_iter()
                .filter(|contract| {
                    contract.kind == ContractMatchKind::HttpRoute
                        && contract.provider_repo == provider_name
                        && contract.consumer_repo == consumer_name
                })
                .collect::<Vec<_>>();
            Ok::<_, McpError>((catalog, contracts))
        })
        .await??;
    if contracts.is_empty() {
        return json_result(&serde_json::json!({
            "provider": args.provider,
            "consumer": args.consumer,
            "contracts": [],
            "note": "No HTTP contracts found between these repos in the group."
        }));
    }

    let provider_repo = catalog
        .resolve(RepoSelector::NameOrPath(args.provider.clone()))
        .map_err(app_error_to_mcp)?;
    let consumer_repo = catalog
        .resolve(RepoSelector::NameOrPath(args.consumer.clone()))
        .map_err(app_error_to_mcp)?;
    let provider_snapshot = artifacts
        .snapshot(&provider_repo)
        .await
        .map_err(app_error_to_mcp)?;
    let consumer_snapshot = artifacts
        .snapshot(&consumer_repo)
        .await
        .map_err(app_error_to_mcp)?;
    run_blocking_heavy(blocking_timeout(), "shape_check analysis", move || {
        shape_check_loaded(
            args,
            catalog,
            contracts,
            provider_snapshot,
            consumer_snapshot,
        )
    })
    .await?
}

fn shape_check_loaded(
    args: ShapeCheckArgs,
    catalog: RepoCatalogSnapshot,
    contracts: Vec<ContractMatch>,
    provider_snapshot: Arc<ArtifactSnapshot>,
    consumer_snapshot: Arc<ArtifactSnapshot>,
) -> Result<CallToolResult, McpError> {
    let provider_nodes = provider_snapshot.nodes.as_ref();
    let consumer_nodes = consumer_snapshot.nodes.as_ref();
    let consumer_edges = consumer_snapshot.edges.as_ref();

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
    for edge in consumer_edges {
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

    let (synced_at, stale) = group_freshness(&args.group, &catalog);
    json_result(&serde_json::json!({
        "provider": args.provider,
        "consumer": args.consumer,
        "contracts": results,
        "contracts_synced_at": synced_at,
        "contracts_stale": stale,
    }))
}

#[cfg(test)]
mod tests {
    use super::validate_group_member;

    #[test]
    fn group_member_accepted() {
        let members = vec!["212ecom-be".to_string(), "212ecom-fe".to_string()];
        assert!(validate_group_member("shop", &members, "212ecom-fe").is_ok());
    }

    #[test]
    fn non_member_rejected_naming_members() {
        let members = vec!["212ecom-be".to_string(), "212ecom-fe".to_string()];
        let err = validate_group_member("shop", &members, "yummy-cih").unwrap_err();
        assert!(err.contains("yummy-cih"));
        assert!(err.contains("shop"));
        assert!(err.contains("212ecom-be") && err.contains("212ecom-fe"));
    }
}
