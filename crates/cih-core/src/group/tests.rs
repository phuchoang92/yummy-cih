use super::*;

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
