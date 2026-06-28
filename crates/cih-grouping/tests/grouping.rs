use cih_grouping::{
    apply_overrides, FeatureGroupEntry, FeatureOverrideEntry, FeatureOverrides,
};

fn entry(node_id: &str, feature: &str) -> FeatureGroupEntry {
    FeatureGroupEntry {
        id: format!("feature:{feature}"),
        name: feature.into(),
        node_id: node_id.into(),
        strategy: "package".into(),
        confidence: 1.0,
        pinned: false,
        evidence: String::new(),
        node_content_hash: 0,
    }
}

#[test]
fn override_replaces_existing_and_pins() {
    let entries = vec![
        entry("Class:com.example.Foo", "shared"),
        entry("Class:com.example.Bar", "payment"),
    ];
    let overrides = FeatureOverrides {
        version: 1,
        entries: vec![FeatureOverrideEntry {
            node_id: "Class:com.example.Foo".into(),
            feature: "overdraft".into(),
            reason: "manual correction".into(),
        }],
    };
    let merged = apply_overrides(entries, &overrides);
    assert_eq!(merged.len(), 2);
    let foo = merged
        .iter()
        .find(|e| e.node_id == "Class:com.example.Foo")
        .unwrap();
    assert_eq!(foo.name, "overdraft");
    assert_eq!(foo.strategy, "override");
    assert!(foo.pinned);
    assert_eq!(foo.evidence, "manual correction");
}

#[test]
fn override_adds_new_node_not_in_entries() {
    let entries = vec![entry("Class:com.example.Foo", "payment")];
    let overrides = FeatureOverrides {
        version: 1,
        entries: vec![FeatureOverrideEntry {
            node_id: "Class:com.example.NewNode".into(),
            feature: "auth".into(),
            reason: String::new(),
        }],
    };
    let merged = apply_overrides(entries, &overrides);
    assert_eq!(merged.len(), 2);
    let new_node = merged
        .iter()
        .find(|e| e.node_id == "Class:com.example.NewNode")
        .unwrap();
    assert_eq!(new_node.name, "auth");
    assert_eq!(new_node.evidence, "manual override");
}
