use cih_core::group::{
    group_contracts_stale, GroupEntry, GroupRegistry, SyncRepoSnapshot, SyncState,
};
use cih_core::{Registry, RegistryEntry, RegistryStats};

#[test]
fn upsert_replaces_not_appends() {
    let mut registry = GroupRegistry::default();
    registry.upsert(GroupEntry {
        name: "banking".into(),
        repos: vec!["orders".into()],
        created_at: "2026-01-01T00:00:00Z".into(),
    });
    registry.upsert(GroupEntry {
        name: "banking".into(),
        repos: vec!["orders".into(), "payments".into()],
        created_at: "2026-01-01T00:00:00Z".into(),
    });
    assert_eq!(registry.groups.len(), 1);
    assert_eq!(registry.groups[0].repos, vec!["orders", "payments"]);
}

#[test]
fn remove_returns_whether_group_existed() {
    let mut registry = GroupRegistry::default();
    registry.upsert(GroupEntry {
        name: "banking".into(),
        repos: Vec::new(),
        created_at: "2026-01-01T00:00:00Z".into(),
    });
    assert!(registry.remove("banking"));
    assert!(!registry.remove("banking"));
}

#[test]
fn groups_containing_selects_by_member() {
    let mut registry = GroupRegistry::default();
    registry.upsert(GroupEntry {
        name: "banking".into(),
        repos: vec!["orders".into(), "payments".into()],
        created_at: "2026-01-01T00:00:00Z".into(),
    });
    registry.upsert(GroupEntry {
        name: "retail".into(),
        repos: vec!["catalog".into()],
        created_at: "2026-01-01T00:00:00Z".into(),
    });
    let names: Vec<&str> = registry
        .groups_containing("orders")
        .map(|g| g.name.as_str())
        .collect();
    assert_eq!(names, vec!["banking"]);
    assert_eq!(registry.groups_containing("unknown").count(), 0);
}

#[test]
fn sync_state_roundtrips_through_group_dir() {
    let dir = tempfile::tempdir().unwrap();
    let state = SyncState {
        synced_at: "2026-07-11T00:00:00Z".into(),
        generation: 3,
        repos: vec![SyncRepoSnapshot {
            name: "orders".into(),
            indexed_at: "2026-07-10T00:00:00Z".into(),
            last_git_head: Some("abc123".into()),
        }],
    };
    state.save(dir.path()).unwrap();
    assert_eq!(SyncState::load(dir.path()), Some(state));
}

#[test]
fn sync_state_load_returns_none_when_missing() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(SyncState::load(dir.path()), None);
}

#[test]
fn sync_state_defaults_tolerate_sparse_json() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sync-state.json"), "{}").unwrap();
    let state = SyncState::load(dir.path()).unwrap();
    assert_eq!(state.generation, 0);
    assert!(state.repos.is_empty());
}

fn registry_with(entries: Vec<RegistryEntry>) -> Registry {
    Registry { entries }
}

fn entry(name: &str, indexed_at: &str, head: Option<&str>) -> RegistryEntry {
    RegistryEntry {
        name: name.into(),
        path: format!("/repos/{name}"),
        graph_key: name.into(),
        artifacts_dir: format!("/repos/{name}/.cih/artifacts"),
        community_artifacts_dir: None,
        indexed_at: indexed_at.into(),
        last_git_head: head.map(str::to_string),
        stats: RegistryStats::default(),
    }
}

fn group(repos: &[&str]) -> GroupEntry {
    GroupEntry {
        name: "banking".into(),
        repos: repos.iter().map(|r| r.to_string()).collect(),
        created_at: "2026-01-01T00:00:00Z".into(),
    }
}

fn stamp(snapshots: Vec<SyncRepoSnapshot>) -> SyncState {
    SyncState {
        synced_at: "2026-07-11T00:00:00Z".into(),
        generation: 1,
        repos: snapshots,
    }
}

fn snap(name: &str, indexed_at: &str, head: Option<&str>) -> SyncRepoSnapshot {
    SyncRepoSnapshot {
        name: name.into(),
        indexed_at: indexed_at.into(),
        last_git_head: head.map(str::to_string),
    }
}

#[test]
fn fresh_stamp_matching_registry_is_not_stale() {
    let registry = registry_with(vec![entry("orders", "t1", Some("h1"))]);
    let state = stamp(vec![snap("orders", "t1", Some("h1"))]);
    assert!(!group_contracts_stale(
        &group(&["orders"]),
        &registry,
        Some(&state),
        true
    ));
}

#[test]
fn unstamped_existing_contracts_are_stale() {
    let registry = registry_with(vec![entry("orders", "t1", Some("h1"))]);
    assert!(group_contracts_stale(
        &group(&["orders"]),
        &registry,
        None,
        true
    ));
}

#[test]
fn never_synced_group_is_not_stale() {
    let registry = registry_with(vec![entry("orders", "t1", Some("h1"))]);
    assert!(!group_contracts_stale(
        &group(&["orders"]),
        &registry,
        None,
        false
    ));
}

#[test]
fn member_missing_from_registry_is_stale() {
    let registry = registry_with(vec![entry("orders", "t1", Some("h1"))]);
    let state = stamp(vec![snap("orders", "t1", Some("h1"))]);
    assert!(group_contracts_stale(
        &group(&["orders", "payments"]),
        &registry,
        Some(&state),
        true
    ));
}

#[test]
fn reindexed_member_is_stale() {
    let registry = registry_with(vec![entry("orders", "t2", Some("h1"))]);
    let state = stamp(vec![snap("orders", "t1", Some("h1"))]);
    assert!(group_contracts_stale(
        &group(&["orders"]),
        &registry,
        Some(&state),
        true
    ));
}

#[test]
fn git_head_drift_is_stale() {
    let registry = registry_with(vec![entry("orders", "t1", Some("h2"))]);
    let state = stamp(vec![snap("orders", "t1", Some("h1"))]);
    assert!(group_contracts_stale(
        &group(&["orders"]),
        &registry,
        Some(&state),
        true
    ));
}

#[test]
fn member_added_after_sync_is_stale() {
    let registry = registry_with(vec![
        entry("orders", "t1", Some("h1")),
        entry("payments", "t1", Some("h1")),
    ]);
    let state = stamp(vec![snap("orders", "t1", Some("h1"))]);
    assert!(group_contracts_stale(
        &group(&["orders", "payments"]),
        &registry,
        Some(&state),
        true
    ));
}
