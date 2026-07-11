//! Cross-service contract sync for repo groups: read every member repo's
//! graph artifacts, match provider routes/topics against consumer endpoints,
//! and write `contracts.jsonl` + a `sync-state.json` freshness stamp under
//! `~/.cih/groups/<name>/`.
//!
//! Lib-layer (called from both `cmd/group_sync.rs` and the registry
//! persistence hooks); the core is parameterized over registries and the
//! output dir so tests never touch `~/.cih`.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use cih_core::{
    ContractMatch, ContractMatchKind, EdgeKind, GraphArtifacts, GroupEntry, GroupRegistry,
    MessagingFramework, Node, NodeKind, Registry, RegistryEntry, SyncState, VersionId,
};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct SyncSummary {
    pub group: String,
    pub repo_count: usize,
    pub contract_count: usize,
    pub output_path: String,
}

#[derive(Clone, Debug)]
pub struct RouteContract {
    pub repo: String,
    pub id: String,
    pub method: String,
    pub path: String,
}

#[derive(Clone, Debug)]
pub struct EndpointContract {
    pub repo: String,
    pub id: String,
    pub method: String,
    pub path: String,
}

#[derive(Clone, Debug)]
#[doc(hidden)]
pub struct EventContract {
    pub repo: String,
    pub caller_id: String,
    pub topic: String,
    /// Messaging framework carried on the contract edge (`None` for pre-field artifacts
    /// or non-Java sources); classification is data, not inferred in the engine.
    pub framework: Option<MessagingFramework>,
}

#[derive(Clone, Debug, Default)]
pub struct RepoContracts {
    pub routes: Vec<RouteContract>,
    pub endpoints: Vec<EndpointContract>,
    pub publishes: Vec<EventContract>,
    pub listens: Vec<EventContract>,
}

/// Sync one group by name, loading the group + repo registries from `~/.cih`.
pub fn sync_group(name: &str) -> Result<SyncSummary> {
    let group_registry = GroupRegistry::load();
    let group = group_registry
        .find(name)
        .ok_or_else(|| anyhow!("group '{name}' does not exist"))?;
    let registry = Registry::load();
    let out_dir =
        cih_core::group_dir(name).ok_or_else(|| anyhow!("cannot determine HOME for group path"))?;
    sync_group_entry(group, &registry, &out_dir)
}

/// Sync core: resolve every member repo, match contracts, write
/// `contracts.jsonl` and the `sync-state.json` freshness stamp into `out_dir`.
pub fn sync_group_entry(
    group: &GroupEntry,
    registry: &Registry,
    out_dir: &Path,
) -> Result<SyncSummary> {
    let mut repos = Vec::new();
    let mut snapshots = Vec::new();
    for repo_name in &group.repos {
        let entry = registry
            .find(repo_name)
            .ok_or_else(|| anyhow!("repo '{repo_name}' is not registered; run analyze first"))?;
        repos.push(load_repo_contracts(entry)?);
        snapshots.push(SyncState::snapshot_of(entry));
    }

    let matches = match_contracts(&repos);
    std::fs::create_dir_all(out_dir)?;
    let output_path = out_dir.join("contracts.jsonl");
    write_jsonl(&output_path, &matches)?;

    let state = SyncState {
        synced_at: cih_core::now_rfc3339(),
        generation: SyncState::load(out_dir).map_or(0, |prev| prev.generation) + 1,
        repos: snapshots,
    };
    state
        .save(out_dir)
        .with_context(|| format!("failed to write sync state for group '{}'", group.name))?;

    Ok(SyncSummary {
        group: group.name.clone(),
        repo_count: repos.len(),
        contract_count: matches.len(),
        output_path: output_path.display().to_string(),
    })
}

/// Re-sync every group that contains `repo_name`. Called from the registry
/// persistence hooks after `analyze`/`discover`; failures are logged and never
/// propagated — indexing one repo must not fail because a sibling repo's
/// artifacts are missing. Disable with `CIH_NO_AUTO_GROUP_SYNC=1`.
pub fn auto_sync_groups_for_repo(groups: &GroupRegistry, registry: &Registry, repo_name: &str) {
    if std::env::var_os("CIH_NO_AUTO_GROUP_SYNC").is_some_and(|v| v == "1") {
        tracing::debug!("auto group sync disabled via CIH_NO_AUTO_GROUP_SYNC");
        return;
    }
    for group in groups.groups_containing(repo_name) {
        let result = cih_core::group_dir(&group.name)
            .ok_or_else(|| anyhow!("cannot determine HOME for group path"))
            .and_then(|out_dir| sync_group_entry(group, registry, &out_dir));
        match result {
            Ok(summary) => tracing::info!(
                group = %group.name,
                contracts = summary.contract_count,
                "auto-synced group contracts"
            ),
            Err(e) => tracing::warn!(
                group = %group.name,
                error = %e,
                "auto group sync failed; run `cih-engine group sync` manually"
            ),
        }
    }
}

fn load_repo_contracts(entry: &RegistryEntry) -> Result<RepoContracts> {
    let artifacts_dir = Path::new(&entry.artifacts_dir);
    let artifacts = GraphArtifacts {
        nodes_path: artifacts_dir.join("nodes.jsonl"),
        edges_path: artifacts_dir.join("edges.jsonl"),
        version: VersionId::new(String::new()),
    };
    let nodes = artifacts
        .read_nodes()
        .with_context(|| format!("failed to read nodes for repo '{}'", entry.name))?;
    let edges = artifacts
        .read_edges()
        .with_context(|| format!("failed to read edges for repo '{}'", entry.name))?;

    let nodes_by_id: BTreeMap<String, Node> = nodes
        .iter()
        .map(|node| (node.id.as_str().to_string(), node.clone()))
        .collect();
    let mut contracts = RepoContracts::default();

    for node in &nodes {
        match node.kind {
            NodeKind::Route => {
                if let (Some(method), Some(path)) = (
                    node_prop_str(node, "httpMethod"),
                    node_prop_str(node, "path"),
                ) {
                    contracts.routes.push(RouteContract {
                        repo: entry.name.clone(),
                        id: node.id.as_str().to_string(),
                        method: method.to_ascii_uppercase(),
                        path,
                    });
                }
            }
            NodeKind::ExternalEndpoint => {
                if let (Some(method), Some(path)) = (
                    node_prop_str(node, "httpMethod"),
                    node_prop_str(node, "urlTemplate").or_else(|| node_prop_str(node, "path")),
                ) {
                    contracts.endpoints.push(EndpointContract {
                        repo: entry.name.clone(),
                        id: node.id.as_str().to_string(),
                        method: method.to_ascii_uppercase(),
                        path,
                    });
                }
            }
            _ => {}
        }
    }

    for edge in edges {
        if !matches!(edge.kind, EdgeKind::PublishesEvent | EdgeKind::ListensTo) {
            continue;
        }
        let Some(topic_node) = nodes_by_id.get(edge.dst.as_str()) else {
            continue;
        };
        if topic_node.kind != NodeKind::KafkaTopic {
            continue;
        }
        let topic = node_prop_str(topic_node, "topic").unwrap_or_else(|| topic_node.name.clone());
        let framework = edge
            .props
            .as_ref()
            .and_then(|p| p.get("messaging_framework").cloned())
            .and_then(|v| serde_json::from_value::<MessagingFramework>(v).ok());
        let contract = EventContract {
            repo: entry.name.clone(),
            caller_id: edge.src.as_str().to_string(),
            topic,
            framework,
        };
        match edge.kind {
            EdgeKind::PublishesEvent => contracts.publishes.push(contract),
            EdgeKind::ListensTo => contracts.listens.push(contract),
            _ => {}
        }
    }

    Ok(contracts)
}

pub fn match_contracts(repos: &[RepoContracts]) -> Vec<ContractMatch> {
    let mut route_providers: BTreeMap<(String, String), Vec<&RouteContract>> = BTreeMap::new();
    let mut event_publishers: BTreeMap<String, Vec<&EventContract>> = BTreeMap::new();
    let mut event_listeners: BTreeMap<String, Vec<&EventContract>> = BTreeMap::new();

    for repo in repos {
        for route in &repo.routes {
            route_providers
                .entry((route.method.clone(), normalize_contract_path(&route.path)))
                .or_default()
                .push(route);
        }
        for event in &repo.publishes {
            event_publishers
                .entry(event.topic.clone())
                .or_default()
                .push(event);
        }
        for event in &repo.listens {
            event_listeners
                .entry(event.topic.clone())
                .or_default()
                .push(event);
        }
    }

    let mut matches = Vec::new();
    for repo in repos {
        for endpoint in &repo.endpoints {
            let path = normalize_contract_path(&endpoint.path);
            // Exact verb first, then method-agnostic providers (Go net/http
            // `HandleFunc` routes register as "ANY" and would never match a
            // concrete consumer verb otherwise).
            let mut candidates: Vec<&RouteContract> = Vec::new();
            if let Some(providers) = route_providers.get(&(endpoint.method.clone(), path.clone())) {
                candidates.extend(providers.iter().copied());
            }
            if endpoint.method != "ANY" {
                if let Some(providers) = route_providers.get(&("ANY".to_string(), path.clone())) {
                    candidates.extend(providers.iter().copied());
                }
            }
            for provider in candidates {
                if provider.repo == endpoint.repo {
                    continue;
                }
                matches.push(ContractMatch {
                    kind: ContractMatchKind::HttpRoute,
                    provider_repo: provider.repo.clone(),
                    provider_id: provider.id.clone(),
                    consumer_repo: endpoint.repo.clone(),
                    consumer_id: endpoint.id.clone(),
                    match_key: format!("{} {path}", endpoint.method),
                });
            }
        }
    }

    for (topic, publishers) in event_publishers {
        let Some(listeners) = event_listeners.get(&topic) else {
            continue;
        };
        for publisher in publishers {
            for listener in listeners {
                if publisher.repo == listener.repo {
                    continue;
                }
                matches.push(ContractMatch {
                    kind: event_match_kind(publisher, listener),
                    provider_repo: publisher.repo.clone(),
                    provider_id: publisher.caller_id.clone(),
                    consumer_repo: listener.repo.clone(),
                    consumer_id: listener.caller_id.clone(),
                    match_key: topic.clone(),
                });
            }
        }
    }

    dedup_matches(matches)
}

fn event_match_kind(provider: &EventContract, consumer: &EventContract) -> ContractMatchKind {
    // The framework travels with the contract (set by the parser); the engine just reads it.
    // Default to KafkaTopic for pre-field artifacts / edges without a framework tag.
    provider
        .framework
        .or(consumer.framework)
        .map(ContractMatchKind::from)
        .unwrap_or(ContractMatchKind::KafkaTopic)
}

fn dedup_matches(matches: Vec<ContractMatch>) -> Vec<ContractMatch> {
    let mut seen = std::collections::HashSet::new();
    matches
        .into_iter()
        .filter(|m| seen.insert(m.clone()))
        .collect()
}

pub fn normalize_contract_path(path: &str) -> String {
    cih_core::normalize_contract_path(path)
}

fn node_prop_str(node: &Node, key: &str) -> Option<String> {
    node.props
        .as_ref()
        .and_then(|props| props.get(key))
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn write_jsonl(path: &Path, matches: &[ContractMatch]) -> Result<()> {
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    for item in matches {
        serde_json::to_writer(&mut writer, item)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(repo: &str, topic: &str, fw: Option<MessagingFramework>) -> EventContract {
        EventContract {
            repo: repo.to_string(),
            caller_id: format!("Method:{repo}#handler/1"),
            topic: topic.to_string(),
            framework: fw,
        }
    }

    fn matched_kind(publisher: EventContract, listener: EventContract) -> ContractMatchKind {
        let provider = RepoContracts {
            publishes: vec![publisher],
            ..Default::default()
        };
        let consumer = RepoContracts {
            listens: vec![listener],
            ..Default::default()
        };
        let matches = match_contracts(&[provider, consumer]);
        assert_eq!(matches.len(), 1, "expected exactly one event match");
        matches[0].kind
    }

    #[test]
    fn spring_framework_yields_spring_event_match() {
        let kind = matched_kind(
            event("svc-a", "OrderPlaced", Some(MessagingFramework::Spring)),
            event("svc-b", "OrderPlaced", Some(MessagingFramework::Spring)),
        );
        assert_eq!(kind, ContractMatchKind::SpringEvent);
    }

    #[test]
    fn kafka_framework_yields_kafka_topic_match() {
        let kind = matched_kind(
            event("svc-a", "orders", Some(MessagingFramework::Kafka)),
            event("svc-b", "orders", Some(MessagingFramework::Kafka)),
        );
        assert_eq!(kind, ContractMatchKind::KafkaTopic);
    }

    #[test]
    fn missing_framework_defaults_to_kafka_topic() {
        let kind = matched_kind(
            event("svc-a", "orders", None),
            event("svc-b", "orders", None),
        );
        assert_eq!(kind, ContractMatchKind::KafkaTopic);
    }

    #[test]
    fn provider_framework_used_when_consumer_missing() {
        let kind = matched_kind(
            event("svc-a", "OrderPlaced", Some(MessagingFramework::Spring)),
            event("svc-b", "OrderPlaced", None),
        );
        assert_eq!(kind, ContractMatchKind::SpringEvent);
    }

    #[test]
    fn consumer_framework_used_when_provider_missing() {
        let kind = matched_kind(
            event("svc-a", "orders", None),
            event("svc-b", "orders", Some(MessagingFramework::Kafka)),
        );
        assert_eq!(kind, ContractMatchKind::KafkaTopic);
    }
}
