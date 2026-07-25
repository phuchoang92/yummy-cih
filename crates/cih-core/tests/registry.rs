use cih_core::registry::{unix_secs_to_rfc3339, Registry, RegistryEntry, RegistryStats};

#[test]
fn load_missing_returns_empty() {
    let _ = Registry::load();
}

#[test]
fn upsert_replaces_not_appends() {
    let mut reg = Registry::default();
    let base = RegistryEntry {
        name: "foo".into(),
        path: "/tmp/foo".into(),
        graph_key: "cih".into(),
        artifacts_dir: "/tmp/foo/.cih/artifacts/v1".into(),
        community_artifacts_dir: None,
        indexed_at: "2026-01-01T00:00:00Z".into(),
        last_git_head: None,
        stats: RegistryStats::default(),
    };
    reg.upsert(base.clone());
    reg.upsert(RegistryEntry {
        artifacts_dir: "/tmp/foo/.cih/artifacts/v2".into(),
        ..base
    });
    assert_eq!(reg.entries.len(), 1);
    assert_eq!(reg.entries[0].artifacts_dir, "/tmp/foo/.cih/artifacts/v2");
}

#[test]
fn rfc3339_epoch() {
    assert_eq!(unix_secs_to_rfc3339(0), "1970-01-01T00:00:00Z");
}

#[test]
fn rfc3339_one_day() {
    assert_eq!(unix_secs_to_rfc3339(86400), "1970-01-02T00:00:00Z");
}

#[test]
fn legacy_registry_stats_mark_route_count_stale() {
    let stats: RegistryStats = serde_json::from_value(serde_json::json!({
        "nodes": 10,
        "edges": 20,
        "files": 3,
        "routes": 0,
        "communities": 0,
        "processes": 0
    }))
    .unwrap();

    assert_eq!(stats.routes, 0);
    assert!(!stats.routes_current);
}
