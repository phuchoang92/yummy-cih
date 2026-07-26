use std::sync::Arc;

use cih_core::registry::{
    ensure_repository_id, graph_content_version, new_publication_epoch, unix_secs_to_rfc3339,
    Registry, RegistryEntry, RegistryGraphReport, RegistryStats, RegistryStore, RepositoryId,
};
use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};

fn entry(name: &str, version: &str) -> RegistryEntry {
    RegistryEntry {
        repository_id: None,
        name: name.into(),
        path: format!("/tmp/{name}"),
        graph_key: name.into(),
        artifacts_dir: format!("/tmp/{name}/.cih/artifacts/{version}"),
        latest_artifact_version: None,
        published_artifact_version: None,
        published_graph_content_version: None,
        published_epoch: None,
        community_artifacts_dir: None,
        indexed_at: "2026-01-01T00:00:00Z".into(),
        last_git_head: None,
        stats: RegistryStats::default(),
    }
}

fn store_fixture() -> (tempfile::TempDir, RegistryStore) {
    let temp = tempfile::tempdir().unwrap();
    let store = RegistryStore::new(temp.path().join("registry.json"));
    (temp, store)
}

fn repository_id(digit: char) -> RepositoryId {
    RepositoryId::parse(std::iter::repeat_n(digit, 64).collect::<String>()).unwrap()
}

#[test]
fn load_missing_returns_empty_snapshot() {
    let (_temp, store) = store_fixture();
    let snapshot = store.load().unwrap();
    assert!(snapshot.registry.entries.is_empty());
    assert_eq!(snapshot.revision.sequence, 0);
    assert!(snapshot.revision.content_digest.is_empty());
    assert!(!snapshot.recovered_from_backup);
}

#[test]
fn upsert_replaces_not_appends() {
    let mut reg = Registry::default();
    let base = entry("foo", "v1");
    reg.upsert(base.clone());
    reg.upsert(RegistryEntry {
        artifacts_dir: "/tmp/foo/.cih/artifacts/v2".into(),
        ..base
    });
    assert_eq!(reg.entries.len(), 1);
    assert_eq!(reg.entries[0].artifacts_dir, "/tmp/foo/.cih/artifacts/v2");
}

#[test]
fn legacy_registry_migrates_identity_and_latest_version_once() {
    let (temp, store) = store_fixture();
    let legacy = Registry {
        entries: vec![entry("legacy", "v1")],
    };
    std::fs::write(
        temp.path().join("registry.json"),
        serde_json::to_vec_pretty(&legacy).unwrap(),
    )
    .unwrap();

    let snapshot = store.load().unwrap();
    assert_eq!(snapshot.registry.entries.len(), 1);
    assert_eq!(snapshot.revision.sequence, 1);
    assert_eq!(snapshot.revision.content_digest.len(), 64);
    let migrated = &snapshot.registry.entries[0];
    let repository_id = migrated.repository_id.clone().unwrap();
    assert_eq!(migrated.latest_artifact_version.as_deref(), Some("v1"));
    assert!(migrated.published_artifact_version.is_none());
    assert!(migrated.published_graph_content_version.is_none());
    assert!(migrated.published_epoch.is_none());

    let second = store.load().unwrap();
    assert_eq!(second.revision, snapshot.revision);
    assert_eq!(
        second.registry.entries[0].repository_id.as_ref(),
        Some(&repository_id)
    );

    let primary: serde_json::Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("registry.json")).unwrap()).unwrap();
    let backup: serde_json::Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("registry.json.bak")).unwrap())
            .unwrap();
    assert_eq!(
        primary["entries"][0]["repository_id"],
        backup["entries"][0]["repository_id"]
    );
}

#[test]
fn sparse_legacy_entry_deserializes_without_identity_or_publication_claims() {
    let entry: RegistryEntry = serde_json::from_value(serde_json::json!({
        "name": "legacy",
        "path": "/repos/legacy",
        "graph_key": "legacy",
        "artifacts_dir": "/repos/legacy/.cih/artifacts/v1",
        "indexed_at": "2026-01-01T00:00:00Z",
        "stats": {
            "nodes": 1,
            "edges": 0,
            "files": 1,
            "routes": 0,
            "communities": 0,
            "processes": 0
        }
    }))
    .unwrap();

    assert!(entry.repository_id.is_none());
    assert!(entry.latest_artifact_version.is_none());
    assert!(entry.published_artifact_version.is_none());
    assert!(entry.published_graph_content_version.is_none());
    assert!(entry.published_epoch.is_none());
}

#[test]
fn transaction_writes_revision_full_digest_and_backup() {
    let (temp, store) = store_fixture();
    let update = store
        .update(|registry| {
            registry.upsert(entry("orders", "v1"));
            Ok(())
        })
        .unwrap();

    assert!(update.changed);
    assert_eq!(update.snapshot.revision.sequence, 1);
    assert_eq!(update.snapshot.revision.content_digest.len(), 64);
    assert!(update
        .snapshot
        .revision
        .content_digest
        .chars()
        .all(|character| character.is_ascii_hexdigit()));
    assert!(temp.path().join("registry.json.bak").is_file());

    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("registry.json")).unwrap()).unwrap();
    assert_eq!(raw["revision"], 1);
    assert_eq!(raw["content_digest"].as_str().unwrap().len(), 64);
}

#[test]
fn equal_content_is_a_noop_and_keeps_revision() {
    let (_temp, store) = store_fixture();
    let first = store
        .update(|registry| {
            registry.upsert(entry("orders", "v1"));
            Ok(())
        })
        .unwrap();
    let second = store.update(|_registry| Ok("unchanged")).unwrap();

    assert!(!second.changed);
    assert_eq!(second.value, "unchanged");
    assert_eq!(second.snapshot.revision, first.snapshot.revision);
}

#[test]
fn canonical_digest_is_independent_of_entry_insertion_order() {
    let (_left_temp, left) = store_fixture();
    let (_right_temp, right) = store_fixture();
    let orders = entry("orders", "v1");
    let payments = entry("payments", "v1");
    let mut orders = orders;
    orders.repository_id = Some(repository_id('a'));
    let mut payments = payments;
    payments.repository_id = Some(repository_id('b'));

    let left = left
        .update(|registry| {
            registry.entries = vec![orders.clone(), payments.clone()];
            Ok(())
        })
        .unwrap();
    let right = right
        .update(|registry| {
            registry.entries = vec![payments, orders];
            Ok(())
        })
        .unwrap();

    assert_eq!(
        left.snapshot.revision.content_digest,
        right.snapshot.revision.content_digest
    );
}

#[test]
fn compatibility_upsert_preserves_immutable_identity_and_published_state() {
    let mut registry = Registry::default();
    let mut published = entry("orders", "v1");
    published.repository_id = Some(repository_id('c'));
    published.latest_artifact_version = Some("v1".into());
    published.published_artifact_version = Some("v1".into());
    published.published_graph_content_version = Some(graph_content_version("v1", &[]));
    published.published_epoch = Some(new_publication_epoch());
    let expected = published.clone();
    registry.upsert(published);

    registry.upsert(entry("orders", "v2"));
    let updated = registry.find("orders").unwrap();
    assert_eq!(updated.repository_id, expected.repository_id);
    assert_eq!(updated.latest_artifact_version.as_deref(), Some("v2"));
    assert_eq!(
        updated.published_artifact_version,
        expected.published_artifact_version
    );
    assert_eq!(
        updated.published_graph_content_version,
        expected.published_graph_content_version
    );
    assert_eq!(updated.published_epoch, expected.published_epoch);
}

#[test]
fn concurrent_legacy_reads_allocate_exactly_one_repository_identity() {
    const READERS: usize = 16;
    let (temp, store) = store_fixture();
    let legacy = Registry {
        entries: vec![entry("legacy", "v1")],
    };
    std::fs::write(
        temp.path().join("registry.json"),
        serde_json::to_vec_pretty(&legacy).unwrap(),
    )
    .unwrap();
    let store = Arc::new(store);
    let handles = (0..READERS)
        .map(|_| {
            let store = store.clone();
            std::thread::spawn(move || {
                store.load().unwrap().registry.entries[0]
                    .repository_id
                    .clone()
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();

    let mut identities = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    identities.dedup();
    assert_eq!(identities.len(), 1);
    assert_eq!(store.load().unwrap().revision.sequence, 1);
}

#[test]
fn newer_migration_backup_repairs_an_interrupted_primary() {
    let (temp, store) = store_fixture();
    let legacy = Registry {
        entries: vec![entry("legacy", "v1")],
    };
    let legacy_bytes = serde_json::to_vec_pretty(&legacy).unwrap();
    let primary_path = temp.path().join("registry.json");
    std::fs::write(&primary_path, &legacy_bytes).unwrap();
    let migrated = store.load().unwrap();
    let expected_id = migrated.registry.entries[0].repository_id.clone();

    // This is the durable state if migration wrote its newer backup and the
    // process died before replacing the old primary.
    std::fs::write(&primary_path, legacy_bytes).unwrap();
    let recovered = store.load().unwrap();
    assert!(recovered.recovered_from_backup);
    assert_eq!(recovered.revision.sequence, 1);
    assert_eq!(recovered.registry.entries[0].repository_id, expected_id);

    let repaired = store.load().unwrap();
    assert!(!repaired.recovered_from_backup);
    assert_eq!(repaired.registry.entries[0].repository_id, expected_id);
}

#[test]
fn concurrent_compatibility_updates_cannot_change_identity_or_publication() {
    const WRITERS: usize = 12;
    let (_temp, store) = store_fixture();
    store
        .update(|registry| {
            let mut published = entry("orders", "v1");
            published.repository_id = Some(repository_id('d'));
            published.latest_artifact_version = Some("v1".into());
            published.published_artifact_version = Some("v1".into());
            published.published_graph_content_version = Some(graph_content_version("v1", &[]));
            published.published_epoch = Some(new_publication_epoch());
            registry.upsert(published);
            Ok(())
        })
        .unwrap();
    let expected = store.load().unwrap().registry.entries[0].clone();
    let store = Arc::new(store);
    let handles = (0..WRITERS)
        .map(|index| {
            let store = store.clone();
            std::thread::spawn(move || {
                store
                    .update(|registry| {
                        registry.upsert(entry("orders", &format!("candidate-{index}")));
                        Ok(())
                    })
                    .unwrap();
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        handle.join().unwrap();
    }

    let actual = store.load().unwrap().registry.entries[0].clone();
    assert_eq!(actual.repository_id, expected.repository_id);
    assert_eq!(
        actual.published_artifact_version,
        expected.published_artifact_version
    );
    assert_eq!(
        actual.published_graph_content_version,
        expected.published_graph_content_version
    );
    assert_eq!(actual.published_epoch, expected.published_epoch);
}

#[test]
fn repository_identity_record_is_idempotent_and_rejects_conflicts() {
    let repo = tempfile::tempdir().unwrap();
    let preferred = repository_id('e');
    let assigned = ensure_repository_id(repo.path(), Some(&preferred)).unwrap();
    assert_eq!(assigned, preferred);
    assert_eq!(ensure_repository_id(repo.path(), None).unwrap(), preferred);

    let error = ensure_repository_id(repo.path(), Some(&repository_id('f')))
        .unwrap_err()
        .to_string();
    assert!(error.contains("identity conflict"), "{error}");
}

#[test]
fn transactional_direct_mutation_cannot_replace_repository_identity() {
    let (_temp, store) = store_fixture();
    store
        .update(|registry| {
            let mut registered = entry("orders", "v1");
            registered.repository_id = Some(repository_id('1'));
            registry.upsert(registered);
            Ok(())
        })
        .unwrap();

    let error = store
        .update(|registry| {
            registry.entries[0].repository_id = Some(repository_id('2'));
            Ok(())
        })
        .unwrap_err()
        .to_string();
    assert!(error.contains("immutable"), "{error}");
    assert_eq!(
        store.load().unwrap().registry.entries[0]
            .repository_id
            .as_ref(),
        Some(&repository_id('1'))
    );
}

#[test]
fn graph_content_version_is_full_order_sensitive_and_epoch_is_fresh() {
    let left = graph_content_version("base", &[("community", "v1"), ("taint", "v2")]);
    let right = graph_content_version("base", &[("taint", "v2"), ("community", "v1")]);
    assert_eq!(left.len(), 64);
    assert_ne!(left, right);

    let first_epoch = new_publication_epoch();
    let second_epoch = new_publication_epoch();
    assert_eq!(first_epoch.len(), 64);
    assert_ne!(first_epoch, second_epoch);
}

#[test]
fn concurrent_transactions_reread_under_lock_without_lost_updates() {
    const WRITERS: usize = 24;
    let (_temp, store) = store_fixture();
    let store = Arc::new(store);
    let mut handles = Vec::new();

    for index in 0..WRITERS {
        let store = store.clone();
        handles.push(std::thread::spawn(move || {
            store
                .update(|registry| {
                    registry.upsert(entry(&format!("repo-{index}"), "v1"));
                    Ok(())
                })
                .unwrap();
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }

    let snapshot = store.load().unwrap();
    assert_eq!(snapshot.registry.entries.len(), WRITERS);
    assert_eq!(snapshot.revision.sequence, WRITERS as u64);
}

#[test]
fn malformed_primary_recovers_last_known_good_backup() {
    let (temp, store) = store_fixture();
    store
        .update(|registry| {
            registry.upsert(entry("orders", "v1"));
            Ok(())
        })
        .unwrap();
    store
        .update(|registry| {
            registry.upsert(entry("orders", "v2"));
            Ok(())
        })
        .unwrap();
    std::fs::write(temp.path().join("registry.json"), b"{truncated").unwrap();

    let recovered = store.load().unwrap();
    assert!(recovered.recovered_from_backup);
    assert_eq!(recovered.revision.sequence, 1);
    assert!(recovered.registry.entries[0].artifacts_dir.ends_with("/v1"));

    let repaired = store
        .update(|registry| {
            registry.upsert(entry("orders", "v3"));
            Ok(())
        })
        .unwrap();
    assert!(!repaired.snapshot.recovered_from_backup);
    assert_eq!(repaired.snapshot.revision.sequence, 2);
    assert!(store.load().unwrap().registry.entries[0]
        .artifacts_dir
        .ends_with("/v3"));
}

#[test]
fn digest_mismatch_recovers_backup_instead_of_accepting_tampering() {
    let (temp, store) = store_fixture();
    store
        .update(|registry| {
            registry.upsert(entry("orders", "v1"));
            Ok(())
        })
        .unwrap();
    store
        .update(|registry| {
            registry.upsert(entry("orders", "v2"));
            Ok(())
        })
        .unwrap();

    let path = temp.path().join("registry.json");
    let mut raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    raw["entries"][0]["name"] = serde_json::Value::String("tampered".into());
    std::fs::write(&path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();

    let recovered = store.load().unwrap();
    assert!(recovered.recovered_from_backup);
    assert_eq!(recovered.registry.entries[0].name, "orders");
    assert!(recovered.registry.entries[0].artifacts_dir.ends_with("/v1"));
}

#[test]
fn corrupt_primary_and_backup_return_error_not_empty_registry() {
    let (temp, store) = store_fixture();
    store
        .update(|registry| {
            registry.upsert(entry("orders", "v1"));
            Ok(())
        })
        .unwrap();
    std::fs::write(temp.path().join("registry.json"), b"bad primary").unwrap();
    std::fs::write(temp.path().join("registry.json.bak"), b"bad backup").unwrap();

    let error = store.load().unwrap_err().to_string();
    assert!(error.contains("primary"), "{error}");
    assert!(error.contains("backup"), "{error}");
}

#[test]
fn committed_transactions_leave_no_temporary_files() {
    let (temp, store) = store_fixture();
    store
        .update(|registry| {
            registry.upsert(entry("orders", "v1"));
            Ok(())
        })
        .unwrap();

    let names = std::fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        names.iter().all(|name| !name.contains(".tmp.")),
        "{names:?}"
    );
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
    assert!(stats.published_graph_report.is_none());
}

fn graph_node(id: &str, kind: NodeKind) -> Node {
    Node {
        id: NodeId::new(id),
        kind,
        name: id.to_string(),
        qualified_name: None,
        file: "Fixture.java".to_string(),
        range: Range::default(),
        props: None,
    }
}

#[test]
fn graph_report_is_exact_deterministic_and_content_bound() {
    let class = graph_node("Class:A", NodeKind::Class);
    let method = graph_node("Method:A#run/0", NodeKind::Method);
    let route = graph_node("Route:GET:/a", NodeKind::Route);
    let edges = vec![
        Edge::new(
            class.id.clone(),
            method.id.clone(),
            EdgeKind::HasMethod,
            1.0,
            String::new(),
        ),
        Edge::new(
            route.id.clone(),
            method.id.clone(),
            EdgeKind::HandlesRoute,
            1.0,
            String::new(),
        ),
    ];
    let nodes = vec![route, method, class];

    let report = RegistryGraphReport::try_build("content-v1".into(), &[&nodes], &[&edges])
        .expect("unique graph produces report");
    assert_eq!(report.total_nodes, 3);
    assert_eq!(report.total_edges, 2);
    assert_eq!(report.kinds[0].kind, "Class");
    assert_eq!(report.kinds.iter().map(|kind| kind.count).sum::<u64>(), 3);
    assert_eq!(report.symbol_hubs[0].node.id.as_str(), "Method:A#run/0");
    assert_eq!(report.symbol_hubs[0].degree, 2);
    assert!(report.matches_content("content-v1"));
    assert!(!report.matches_content("content-v2"));
}

#[test]
fn graph_report_refuses_unproven_duplicate_or_dangling_artifacts() {
    let node = graph_node("Method:A#run/0", NodeKind::Method);
    let duplicate_nodes = vec![node.clone(), node.clone()];
    assert!(
        RegistryGraphReport::try_build("v".into(), &[&duplicate_nodes], &[])
            .expect_err("duplicate nodes are not exact")
            .contains("duplicate node")
    );

    let nodes = vec![node.clone()];
    let dangling = vec![Edge::new(
        node.id,
        NodeId::new("Method:Missing#run/0"),
        EdgeKind::Calls,
        1.0,
        String::new(),
    )];
    assert!(
        RegistryGraphReport::try_build("v".into(), &[&nodes], &[&dangling])
            .expect_err("dangling edge is not exact")
            .contains("missing endpoint")
    );
}
