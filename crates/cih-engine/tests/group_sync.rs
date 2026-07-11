use std::path::Path;

use cih_core::{
    ContractMatchKind, GroupEntry, GroupRegistry, Registry, RegistryEntry, RegistryStats, SyncState,
};
use cih_engine::cmd::group_sync::*;

#[test]
fn normalizes_route_variables() {
    assert_eq!(
        normalize_contract_path("/api/orders/{id}?debug=true"),
        "/api/orders/{*}"
    );
    assert_eq!(
        normalize_contract_path("http://orders.local/api/orders/:id"),
        "/api/orders/{*}"
    );
}

#[test]
fn matches_http_provider_and_consumer_across_repos() {
    let provider = RepoContracts {
        routes: vec![RouteContract {
            repo: "orders".into(),
            id: "Route:GET /api/orders/{id}".into(),
            method: "GET".into(),
            path: "/api/orders/{id}".into(),
        }],
        ..RepoContracts::default()
    };
    let consumer = RepoContracts {
        endpoints: vec![EndpointContract {
            repo: "checkout".into(),
            id: "ExternalEndpoint:GET:/api/orders/42".into(),
            method: "GET".into(),
            path: "/api/orders/:id".into(),
        }],
        ..RepoContracts::default()
    };

    let matches = match_contracts(&[provider, consumer]);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].kind, ContractMatchKind::HttpRoute);
    assert_eq!(matches[0].provider_repo, "orders");
    assert_eq!(matches[0].consumer_repo, "checkout");
}

/// Write a minimal artifacts dir (nodes.jsonl + edges.jsonl) for a fake repo:
/// one provider Route or one consumer ExternalEndpoint for `GET /api/orders/{id}`.
fn write_repo_artifacts(dir: &Path, provider: bool) {
    std::fs::create_dir_all(dir).unwrap();
    let node = if provider {
        serde_json::json!({
            "id": "Route:GET /api/orders/{id}",
            "kind": "Route",
            "name": "GET /api/orders/{id}",
            "file": "src/OrderController.java",
            "props": {"httpMethod": "GET", "path": "/api/orders/{id}"},
        })
    } else {
        serde_json::json!({
            "id": "ExternalEndpoint:GET:/api/orders/{id}",
            "kind": "ExternalEndpoint",
            "name": "GET /api/orders/{id}",
            "file": "src/client.ts",
            "props": {"httpMethod": "GET", "urlTemplate": "/api/orders/{id}"},
        })
    };
    std::fs::write(dir.join("nodes.jsonl"), format!("{node}\n")).unwrap();
    std::fs::write(dir.join("edges.jsonl"), "").unwrap();
}

fn registry_entry(name: &str, artifacts_dir: &Path, indexed_at: &str) -> RegistryEntry {
    RegistryEntry {
        name: name.into(),
        path: format!("/repos/{name}"),
        graph_key: name.into(),
        artifacts_dir: artifacts_dir.display().to_string(),
        community_artifacts_dir: None,
        indexed_at: indexed_at.into(),
        last_git_head: Some(format!("{name}-head")),
        stats: RegistryStats::default(),
    }
}

fn two_repo_fixture(root: &Path) -> (GroupEntry, Registry) {
    let orders_dir = root.join("orders-artifacts");
    let checkout_dir = root.join("checkout-artifacts");
    write_repo_artifacts(&orders_dir, true);
    write_repo_artifacts(&checkout_dir, false);
    let group = GroupEntry {
        name: "shop".into(),
        repos: vec!["orders".into(), "checkout".into()],
        created_at: "2026-01-01T00:00:00Z".into(),
    };
    let registry = Registry {
        entries: vec![
            registry_entry("orders", &orders_dir, "t1"),
            registry_entry("checkout", &checkout_dir, "t1"),
        ],
    };
    (group, registry)
}

#[test]
fn sync_group_entry_writes_contracts_and_stamp() {
    let tmp = tempfile::tempdir().unwrap();
    let (group, registry) = two_repo_fixture(tmp.path());
    let out_dir = tmp.path().join("groups").join("shop");

    let summary = sync_group_entry(&group, &registry, &out_dir).unwrap();
    assert_eq!(summary.repo_count, 2);
    assert_eq!(summary.contract_count, 1);

    let contracts = std::fs::read_to_string(out_dir.join("contracts.jsonl")).unwrap();
    assert!(contracts.contains("\"provider_repo\":\"orders\""));

    let state = SyncState::load(&out_dir).expect("sync-state.json written");
    assert_eq!(state.generation, 1);
    assert!(!state.synced_at.is_empty());
    let names: Vec<&str> = state.repos.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["orders", "checkout"]);
    assert_eq!(state.repos[0].indexed_at, "t1");
    assert_eq!(state.repos[0].last_git_head.as_deref(), Some("orders-head"));

    // Freshly stamped state matches the registry snapshot: not stale.
    assert!(!cih_core::group_contracts_stale(
        &group,
        &registry,
        Some(&state),
        true
    ));

    // Second sync bumps the generation.
    sync_group_entry(&group, &registry, &out_dir).unwrap();
    assert_eq!(SyncState::load(&out_dir).unwrap().generation, 2);
}

#[test]
fn sync_group_entry_fails_without_registered_member() {
    let tmp = tempfile::tempdir().unwrap();
    let group = GroupEntry {
        name: "shop".into(),
        repos: vec!["missing".into()],
        created_at: "2026-01-01T00:00:00Z".into(),
    };
    let out_dir = tmp.path().join("groups").join("shop");
    let err = sync_group_entry(&group, &Registry::default(), &out_dir).unwrap_err();
    assert!(err.to_string().contains("not registered"));
    // Failure happens before any write: no partial contracts, no stamp.
    assert!(!out_dir.exists());
}

#[test]
fn auto_sync_swallows_member_resolution_failures() {
    let groups = GroupRegistry {
        groups: vec![GroupEntry {
            name: "broken".into(),
            repos: vec!["orders".into(), "missing-sibling".into()],
            created_at: "2026-01-01T00:00:00Z".into(),
        }],
    };
    // The sibling repo is unregistered, so the sync errors internally; the
    // hook must swallow it (analyze would otherwise fail).
    auto_sync_groups_for_repo(&groups, &Registry::default(), "orders");
}

#[test]
fn auto_sync_is_a_noop_without_matching_groups() {
    auto_sync_groups_for_repo(&GroupRegistry::default(), &Registry::default(), "orders");
}

#[test]
fn any_method_provider_matches_concrete_consumer_verbs() {
    // Go net/http HandleFunc routes register as method "ANY"; consumers with
    // concrete verbs must still match them.
    let provider = RepoContracts {
        routes: vec![RouteContract {
            repo: "orders".into(),
            id: "Route:go:ANY:/orders".into(),
            method: "ANY".into(),
            path: "/orders".into(),
        }],
        ..RepoContracts::default()
    };
    let consumer = RepoContracts {
        endpoints: vec![
            EndpointContract {
                repo: "checkout".into(),
                id: "ExternalEndpoint:GET:/orders".into(),
                method: "GET".into(),
                path: "/orders".into(),
            },
            EndpointContract {
                repo: "checkout".into(),
                id: "ExternalEndpoint:POST:/orders".into(),
                method: "POST".into(),
                path: "/orders".into(),
            },
        ],
        ..RepoContracts::default()
    };

    let matches = match_contracts(&[provider, consumer]);
    assert_eq!(matches.len(), 2, "both verbs match the ANY provider");
    let keys: Vec<&str> = matches.iter().map(|m| m.match_key.as_str()).collect();
    assert!(keys.contains(&"GET /orders"));
    assert!(keys.contains(&"POST /orders"));
}

#[test]
fn exact_verb_provider_does_not_match_other_verbs() {
    let provider = RepoContracts {
        routes: vec![RouteContract {
            repo: "orders".into(),
            id: "Route:GET /orders".into(),
            method: "GET".into(),
            path: "/orders".into(),
        }],
        ..RepoContracts::default()
    };
    let consumer = RepoContracts {
        endpoints: vec![EndpointContract {
            repo: "checkout".into(),
            id: "ExternalEndpoint:POST:/orders".into(),
            method: "POST".into(),
            path: "/orders".into(),
        }],
        ..RepoContracts::default()
    };
    assert!(match_contracts(&[provider, consumer]).is_empty());
}
