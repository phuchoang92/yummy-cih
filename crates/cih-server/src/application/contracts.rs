//! Typed cross-repository application use cases.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use cih_core::{ContractMatch, ContractMatchKind, EdgeKind, NodeKind};
use serde::Serialize;

use crate::app_error::AppError;
use crate::artifact_cache::{ArtifactRepository, ArtifactSnapshot};
use crate::blocking::{blocking_timeout, run_blocking_heavy, BlockingError};
use crate::repo_context::{RepoCatalogSnapshot, RepoContextProvider, RepoSelector};
use crate::xflow::{self, XflowState};

#[derive(Clone)]
pub(crate) struct ContractService {
    repo_contexts: Arc<dyn RepoContextProvider>,
    xflow: XflowState,
    artifacts: Arc<dyn ArtifactRepository>,
}

impl ContractService {
    pub(crate) fn new(
        repo_contexts: Arc<dyn RepoContextProvider>,
        xflow: XflowState,
        artifacts: Arc<dyn ArtifactRepository>,
    ) -> Self {
        Self {
            repo_contexts,
            xflow,
            artifacts,
        }
    }
}

#[derive(Debug)]
pub(crate) struct GroupContractsCommand {
    group: String,
    kind: Option<ContractMatchKind>,
}

impl GroupContractsCommand {
    pub(crate) fn try_new(group: String, kind: String) -> Result<Self, AppError> {
        let group = required("group", group)?;
        let kind = parse_contract_kind(&kind).map_err(|message| AppError::InvalidInput {
            field: "kind",
            message,
        })?;
        Ok(Self { group, kind })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct GroupContractsOutput {
    group: String,
    contracts_synced_at: Option<String>,
    contracts_stale: bool,
    matches: Vec<ContractMatch>,
}

#[derive(Debug)]
pub(crate) struct ApiImpactCommand {
    group: String,
    method: String,
    path: String,
    include_callers: bool,
    caller_depth: u32,
}

impl ApiImpactCommand {
    pub(crate) fn try_new(
        group: String,
        method: String,
        path: String,
        include_callers: bool,
        caller_depth: u32,
    ) -> Result<Self, AppError> {
        let group = required("group", group)?;
        let method = required("method", method)?.to_ascii_uppercase();
        if !matches!(
            method.as_str(),
            "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
        ) {
            return Err(AppError::InvalidInput {
                field: "method",
                message: format!("unsupported HTTP method '{method}'"),
            });
        }
        let path = required("path", path)?;
        let caller_depth = (if caller_depth == 0 { 3 } else { caller_depth }).clamp(1, 6);
        Ok(Self {
            group,
            method,
            path,
            include_callers,
            caller_depth,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ApiImpactOutput {
    method: String,
    path: String,
    match_key: String,
    consumers: Vec<ApiConsumerImpact>,
    contracts_synced_at: Option<String>,
    contracts_stale: bool,
}

#[derive(Debug, Serialize)]
struct ApiConsumerImpact {
    provider_repo: String,
    provider_route: String,
    consumer_repo: String,
    consumer_endpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    consumer_callers: Option<ConsumerCallers>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ConsumerCallers {
    Found(Vec<ConsumerCaller>),
    Unavailable { error: String },
}

fn required(field: &'static str, value: String) -> Result<String, AppError> {
    let value = value.trim().to_string();
    if value.is_empty() {
        Err(AppError::InvalidInput {
            field,
            message: "must not be empty".into(),
        })
    } else {
        Ok(value)
    }
}

fn parse_contract_kind(kind: &str) -> Result<Option<ContractMatchKind>, String> {
    match kind.trim().to_ascii_lowercase().as_str() {
        "" | "all" => Ok(None),
        "http" | "http_route" | "http-route" => Ok(Some(ContractMatchKind::HttpRoute)),
        "kafka" | "kafka_topic" | "kafka-topic" => Ok(Some(ContractMatchKind::KafkaTopic)),
        "spring" | "spring_event" | "spring-event" => Ok(Some(ContractMatchKind::SpringEvent)),
        other => Err(format!(
            "unknown contract kind '{other}'; expected all, http, kafka, or spring"
        )),
    }
}

fn node_prop_str_owned(node: &cih_core::Node, key: &str) -> Option<String> {
    node.props.as_ref()?.get(key)?.as_str().map(str::to_owned)
}

fn strip_response_wrapper(raw: &str) -> &str {
    raw.find('<')
        .and_then(|start| raw.rfind('>').map(|end| &raw[start + 1..end]))
        .unwrap_or(raw)
}

fn short_class_name(fqcn: &str) -> &str {
    fqcn.rsplit('.').next().unwrap_or(fqcn)
}

fn blocking_error(error: BlockingError) -> AppError {
    AppError::Unavailable {
        dependency: "blocking runtime",
        message: error.to_string(),
        retryable: true,
    }
}

fn malformed_contract(error: serde_json::Error) -> AppError {
    AppError::Unavailable {
        dependency: "contracts artifact",
        message: format!("malformed contracts artifact: {error}"),
        retryable: false,
    }
}

/// Read and parse a group's synced contracts, with the canonical
/// "run group sync first" error when they don't exist yet.
fn load_group_contracts(group: &str) -> Result<Vec<ContractMatch>, AppError> {
    let path = cih_core::contracts_path(group).ok_or_else(|| AppError::Unavailable {
        dependency: "contracts path",
        message: "cannot determine HOME for group contracts path".into(),
        retryable: false,
    })?;
    let raw = std::fs::read_to_string(&path).map_err(|e| AppError::InvalidInput {
        field: "group",
        message: format!(
            "cannot read contracts for group '{group}': {e}. \
                 Run `cih-engine group sync {group}` first"
        ),
    })?;
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<ContractMatch>(line).map_err(malformed_contract))
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
impl ContractService {
    pub(crate) async fn group_contracts(
        &self,
        command: GroupContractsCommand,
    ) -> Result<GroupContractsOutput, AppError> {
        let repo_contexts = self.repo_contexts.clone();
        run_blocking_heavy(blocking_timeout(), "group_contracts load", move || {
            let catalog = repo_contexts.catalog_snapshot();
            group_contracts_sync(command, &catalog)
        })
        .await
        .map_err(blocking_error)?
    }
}

fn group_contracts_sync(
    command: GroupContractsCommand,
    catalog: &RepoCatalogSnapshot,
) -> Result<GroupContractsOutput, AppError> {
    let path = cih_core::contracts_path(&command.group).ok_or_else(|| AppError::Unavailable {
        dependency: "contracts path",
        message: "cannot determine HOME for group contracts path".into(),
        retryable: false,
    })?;
    let raw = std::fs::read_to_string(&path).map_err(|e| AppError::InvalidInput {
        field: "group",
        message: format!(
            "cannot read contracts for group '{}' at {}: {e}. Run `cih-engine group sync {}` first",
            command.group,
            path.display(),
            command.group
        ),
    })?;
    let mut matches = Vec::new();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let item: ContractMatch = serde_json::from_str(line).map_err(malformed_contract)?;
        if command.kind.is_none() || command.kind == Some(item.kind) {
            matches.push(item);
        }
    }
    let (contracts_synced_at, contracts_stale) = group_freshness(&command.group, catalog);
    Ok(GroupContractsOutput {
        group: command.group,
        contracts_synced_at,
        contracts_stale,
        matches,
    })
}

impl ContractService {
    pub(crate) async fn api_impact(
        &self,
        command: ApiImpactCommand,
    ) -> Result<ApiImpactOutput, AppError> {
        let group = command.group.clone();
        let repo_contexts = self.repo_contexts.clone();
        let (catalog, contracts) =
            run_blocking_heavy(blocking_timeout(), "api_impact contract load", move || {
                let catalog = repo_contexts.catalog_snapshot();
                let contracts = load_group_contracts(&group)?;
                Ok::<_, AppError>((catalog, contracts))
            })
            .await
            .map_err(blocking_error)??;
        let target_key = format!(
            "{} {}",
            command.method,
            cih_core::normalize_contract_path(&command.path)
        );
        let mut graphs = HashMap::new();
        if command.include_callers {
            for consumer in contracts
                .iter()
                .filter(|item| {
                    item.kind == ContractMatchKind::HttpRoute && item.match_key == target_key
                })
                .map(|item| item.consumer_repo.clone())
                .collect::<HashSet<_>>()
            {
                let loaded = match catalog.resolve(RepoSelector::NameOrPath(consumer.clone())) {
                    Ok(repo) => self
                        .xflow
                        .graph_for(&repo)
                        .await
                        .map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                };
                graphs.insert(consumer, loaded);
            }
        }
        run_blocking_heavy(blocking_timeout(), "api_impact analysis", move || {
            api_impact_sync(command, &catalog, &contracts, &graphs)
        })
        .await
        .map_err(blocking_error)
    }
}

fn api_impact_sync(
    command: ApiImpactCommand,
    catalog: &RepoCatalogSnapshot,
    contracts: &[ContractMatch],
    graphs: &HashMap<String, Result<Arc<xflow::ArtifactGraph>, String>>,
) -> ApiImpactOutput {
    let target_key = format!(
        "{} {}",
        command.method,
        cih_core::normalize_contract_path(&command.path)
    );
    let mut consumers = Vec::new();
    for item in contracts {
        if item.kind != ContractMatchKind::HttpRoute || item.match_key != target_key {
            continue;
        }
        let consumer_callers = if command.include_callers {
            Some(
                match consumer_callers(
                    graphs.get(&item.consumer_repo),
                    &item.consumer_repo,
                    &item.consumer_id,
                    command.caller_depth,
                ) {
                    Ok(callers) => ConsumerCallers::Found(callers),
                    Err(error) => ConsumerCallers::Unavailable { error },
                },
            )
        } else {
            None
        };
        consumers.push(ApiConsumerImpact {
            provider_repo: item.provider_repo.clone(),
            provider_route: item.provider_id.clone(),
            consumer_repo: item.consumer_repo.clone(),
            consumer_endpoint: item.consumer_id.clone(),
            consumer_callers,
        });
    }
    let (contracts_synced_at, contracts_stale) = group_freshness(&command.group, catalog);
    ApiImpactOutput {
        method: command.method,
        path: command.path,
        match_key: target_key,
        consumers,
        contracts_synced_at,
        contracts_stale,
    }
}

#[derive(Debug, Serialize)]
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

#[derive(Debug)]
pub(crate) struct TraceFlowXCommand {
    entry_point: String,
    repo: RepoSelector,
    group: String,
    max_depth: u32,
    max_hops: u32,
}

impl TraceFlowXCommand {
    pub(crate) fn try_new(
        entry_point: String,
        repo: String,
        group: String,
        max_depth: u32,
        max_hops: u32,
    ) -> Result<Self, AppError> {
        Ok(Self {
            entry_point: required("entry_point", entry_point)?,
            repo: RepoSelector::from_wire(&repo),
            group: required("group", group)?,
            max_depth: (if max_depth == 0 {
                xflow::DEFAULT_DEPTH
            } else {
                max_depth
            })
            .clamp(1, xflow::MAX_DEPTH),
            max_hops: (if max_hops == 0 {
                xflow::DEFAULT_HOPS
            } else {
                max_hops
            })
            .clamp(1, 5),
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum TraceFlowXOutput {
    Ambiguous(AmbiguousCandidates),
    Trace(TraceFlowOutput),
}

#[derive(Debug, Serialize)]
pub(crate) struct AmbiguousCandidates {
    status: &'static str,
    candidates: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct TraceFlowOutput {
    entry_point: String,
    repo: String,
    group: String,
    max_depth: u32,
    max_hops: u32,
    contracts_synced_at: Option<String>,
    contracts_stale: bool,
    step_count: usize,
    steps: Vec<xflow::XStep>,
    truncated: Vec<xflow::Truncation>,
}

/// `Err` naming the group's members when `repo_name` is not one of them.
pub(crate) fn validate_group_member(
    group: &str,
    members: &[String],
    repo_name: &str,
) -> Result<(), AppError> {
    if members.iter().any(|member| member == repo_name) {
        return Ok(());
    }
    Err(AppError::InvalidInput {
        field: "repo",
        message: format!(
            "repo '{repo_name}' is not a member of group '{group}' (members: {}) — \
             pass `repo` naming one of them or add it with `cih-engine group add`",
            members.join(", ")
        ),
    })
}

/// Cross-repo downstream trace: walk the start repo's artifacts, hop through
/// the group's contract matches into provider/consumer repos, continue there.
/// The start repo is `args.repo` (registry name/path) or, when empty, the
/// first registry entry bound to the server's graph key.
impl ContractService {
    pub(crate) async fn trace_flow_x(
        &self,
        command: TraceFlowXCommand,
    ) -> Result<TraceFlowXOutput, AppError> {
        let group = command.group.clone();
        let repo_contexts = self.repo_contexts.clone();
        let (catalog, contracts) = run_blocking_heavy(
            blocking_timeout(),
            "trace_flow_x contract load",
            move || {
                let catalog = repo_contexts.catalog_snapshot();
                let contracts = load_group_contracts(&group)?;
                Ok::<_, AppError>((catalog, contracts))
            },
        )
        .await
        .map_err(blocking_error)??;
        let repo = catalog.resolve(command.repo.clone())?;
        let start_repo = repo.registry_entry.name.clone();

        let group_entry =
            catalog
                .groups()
                .find(&command.group)
                .ok_or_else(|| AppError::NotFound {
                    entity: "group",
                    key: command.group.clone(),
                })?;
        validate_group_member(&command.group, &group_entry.repos, &start_repo)?;
        let group_members = group_entry.repos.clone();

        let start_graph = self
        .xflow
        .graph_for(&repo)
        .await
        .map_err(|error| AppError::InvalidInput {
            field: "repo",
            message: format!(
                    "cannot load artifacts for '{start_repo}': {e} — re-run `cih-engine analyze {start_repo}`"
                , e = error
            ),
        })?;

        let entry_id = match xflow::resolve_entry(&start_graph, &command.entry_point) {
            Ok(id) => id,
            Err(candidates) if candidates.is_empty() => {
                return Err(AppError::InvalidInput {
                    field: "entry_point",
                    message: format!(
                        "entry point '{}' not found in repo '{start_repo}'",
                        command.entry_point
                    ),
                });
            }
            Err(candidates) => {
                return Ok(TraceFlowXOutput::Ambiguous(AmbiguousCandidates {
                    status: "ambiguous",
                    candidates,
                }));
            }
        };

        let mut graphs = HashMap::from([(start_repo.clone(), start_graph)]);
        for name in &group_members {
            if graphs.contains_key(name) {
                continue;
            }
            let Ok(repo) = catalog.resolve(RepoSelector::NameOrPath(name.clone())) else {
                continue;
            };
            if let Ok(graph) = self.xflow.graph_for(&repo).await {
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
                command.max_depth,
                command.max_hops,
            );
            let (contracts_synced_at, contracts_stale) = group_freshness(&command.group, &catalog);
            TraceFlowXOutput::Trace(TraceFlowOutput {
                entry_point: entry_id,
                repo: start_repo,
                group: command.group,
                max_depth: command.max_depth,
                max_hops: command.max_hops,
                contracts_synced_at,
                contracts_stale,
                step_count: trace.steps.len(),
                steps: trace.steps,
                truncated: trace.truncated,
            })
        })
        .await
        .map_err(blocking_error)
    }
}

#[derive(Debug)]
pub(crate) struct ShapeCheckCommand {
    group: String,
    provider: String,
    consumer: String,
}

impl ShapeCheckCommand {
    pub(crate) fn try_new(
        group: String,
        provider: String,
        consumer: String,
    ) -> Result<Self, AppError> {
        Ok(Self {
            group: required("group", group)?,
            provider: required("provider", provider)?,
            consumer: required("consumer", consumer)?,
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum ShapeCheckOutput {
    Empty(EmptyShapeCheckOutput),
    Compared(ComparedShapeCheckOutput),
}

#[derive(Debug, Serialize)]
pub(crate) struct EmptyShapeCheckOutput {
    provider: String,
    consumer: String,
    contracts: Vec<ShapeContract>,
    note: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct ComparedShapeCheckOutput {
    provider: String,
    consumer: String,
    contracts: Vec<ShapeContract>,
    contracts_synced_at: Option<String>,
    contracts_stale: bool,
}

#[derive(Debug, Serialize)]
struct ShapeContract {
    provider_route: String,
    consumer_endpoint: String,
    provider_handler: Option<String>,
    provider_return_type: Option<String>,
    provider_dto: Option<String>,
    provider_fields: Vec<String>,
    consumer_accessed_fields: Vec<String>,
    matched: Vec<String>,
    provider_only: Vec<String>,
    consumer_only: Vec<String>,
    note: Option<&'static str>,
}

impl ContractService {
    pub(crate) async fn shape_check(
        &self,
        command: ShapeCheckCommand,
    ) -> Result<ShapeCheckOutput, AppError> {
        let group = command.group.clone();
        let provider_name = command.provider.clone();
        let consumer_name = command.consumer.clone();
        let repo_contexts = self.repo_contexts.clone();
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
                Ok::<_, AppError>((catalog, contracts))
            })
            .await
            .map_err(blocking_error)??;
        if contracts.is_empty() {
            return Ok(ShapeCheckOutput::Empty(EmptyShapeCheckOutput {
                provider: command.provider,
                consumer: command.consumer,
                contracts: Vec::new(),
                note: "No HTTP contracts found between these repos in the group.",
            }));
        }

        let provider_repo = catalog.resolve(RepoSelector::NameOrPath(command.provider.clone()))?;
        let consumer_repo = catalog.resolve(RepoSelector::NameOrPath(command.consumer.clone()))?;
        let provider_snapshot = self.artifacts.snapshot(&provider_repo).await?;
        let consumer_snapshot = self.artifacts.snapshot(&consumer_repo).await?;
        run_blocking_heavy(blocking_timeout(), "shape_check analysis", move || {
            shape_check_loaded(
                command,
                catalog,
                contracts,
                provider_snapshot,
                consumer_snapshot,
            )
        })
        .await
        .map_err(blocking_error)
    }
}

fn shape_check_loaded(
    command: ShapeCheckCommand,
    catalog: RepoCatalogSnapshot,
    contracts: Vec<ContractMatch>,
    provider_snapshot: Arc<ArtifactSnapshot>,
    consumer_snapshot: Arc<ArtifactSnapshot>,
) -> ShapeCheckOutput {
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

        results.push(ShapeContract {
            provider_route: contract.provider_id.clone(),
            consumer_endpoint: contract.consumer_id.clone(),
            provider_handler: handler_sig,
            provider_return_type: return_type_raw.clone(),
            provider_dto: if dto_short.is_empty() {
                None
            } else {
                Some(dto_short.to_string())
            },
            provider_fields,
            consumer_accessed_fields: consumer_accessed.into_iter().collect(),
            matched,
            provider_only,
            consumer_only,
            note: if return_type_raw.is_none() {
                Some("returnType not available — re-run `cih-engine analyze` to populate it")
            } else {
                None
            },
        });
    }

    let (contracts_synced_at, contracts_stale) = group_freshness(&command.group, &catalog);
    ShapeCheckOutput::Compared(ComparedShapeCheckOutput {
        provider: command.provider,
        consumer: command.consumer,
        contracts: results,
        contracts_synced_at,
        contracts_stale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_member_accepted() {
        let members = vec!["212ecom-be".to_string(), "212ecom-fe".to_string()];
        assert!(validate_group_member("shop", &members, "212ecom-fe").is_ok());
    }

    #[test]
    fn non_member_rejected_naming_members() {
        let members = vec!["212ecom-be".to_string(), "212ecom-fe".to_string()];
        let err = validate_group_member("shop", &members, "yummy-cih").unwrap_err();
        let err = err.to_string();
        assert!(err.contains("yummy-cih"));
        assert!(err.contains("shop"));
        assert!(err.contains("212ecom-be") && err.contains("212ecom-fe"));
    }

    #[test]
    fn commands_validate_and_normalize_wire_values() {
        let group = GroupContractsCommand::try_new(" shop ".into(), "HTTP".into()).unwrap();
        assert_eq!(group.group, "shop");
        assert_eq!(group.kind, Some(ContractMatchKind::HttpRoute));

        let impact =
            ApiImpactCommand::try_new("shop".into(), "get".into(), "/orders/{id}".into(), true, 0)
                .unwrap();
        assert_eq!(impact.method, "GET");
        assert_eq!(impact.caller_depth, 3);

        let error =
            ApiImpactCommand::try_new("shop".into(), "CONNECT".into(), "/orders".into(), false, 0)
                .unwrap_err();
        assert!(matches!(
            error,
            AppError::InvalidInput {
                field: "method",
                ..
            }
        ));
    }

    #[test]
    fn typed_outputs_preserve_existing_json_shapes() {
        let group = GroupContractsOutput {
            group: "shop".into(),
            contracts_synced_at: None,
            contracts_stale: true,
            matches: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(group).unwrap(),
            serde_json::json!({
                "group": "shop",
                "contracts_synced_at": null,
                "contracts_stale": true,
                "matches": [],
            })
        );

        let impact = ApiImpactOutput {
            method: "GET".into(),
            path: "/orders/{id}".into(),
            match_key: "GET /orders/{*}".into(),
            consumers: vec![ApiConsumerImpact {
                provider_repo: "orders".into(),
                provider_route: "Route:GET /orders/{id}".into(),
                consumer_repo: "checkout".into(),
                consumer_endpoint: "ExternalEndpoint:GET /orders/{id}".into(),
                consumer_callers: None,
            }],
            contracts_synced_at: Some("2026-07-20T00:00:00Z".into()),
            contracts_stale: false,
        };
        let impact = serde_json::to_value(impact).unwrap();
        assert!(
            impact["consumers"][0]
                .as_object()
                .unwrap()
                .get("consumer_callers")
                .is_none(),
            "consumer_callers must remain absent unless requested"
        );

        let ambiguous = TraceFlowXOutput::Ambiguous(AmbiguousCandidates {
            status: "ambiguous",
            candidates: vec!["Method:a#run/0".into(), "Method:b#run/0".into()],
        });
        assert_eq!(
            serde_json::to_value(ambiguous).unwrap(),
            serde_json::json!({
                "status": "ambiguous",
                "candidates": ["Method:a#run/0", "Method:b#run/0"],
            })
        );

        let empty = ShapeCheckOutput::Empty(EmptyShapeCheckOutput {
            provider: "orders".into(),
            consumer: "checkout".into(),
            contracts: Vec::new(),
            note: "No HTTP contracts found between these repos in the group.",
        });
        assert_eq!(
            serde_json::to_value(empty).unwrap(),
            serde_json::json!({
                "provider": "orders",
                "consumer": "checkout",
                "contracts": [],
                "note": "No HTTP contracts found between these repos in the group.",
            })
        );
    }
}
