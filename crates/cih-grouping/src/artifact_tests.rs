use super::*;

#[test]
fn round_trip_jsonl() {
    let entries = vec![
        FeatureGroupEntry {
            id: "feature:payment".into(),
            name: "payment".into(),
            node_id: "Class:com.example.PaymentService".into(),
            strategy: "package".into(),
            confidence: 1.0,
            pinned: false,
            evidence: "Maven module payment-service".into(),
            node_content_hash: 42,
        },
        FeatureGroupEntry {
            id: "feature:overdraft".into(),
            name: "overdraft".into(),
            node_id: "Class:com.example.OverdraftService".into(),
            strategy: "override".into(),
            confidence: 1.0,
            pinned: true,
            evidence: "manual correction".into(),
            node_content_hash: 0,
        },
    ];

    let jsonl = entries_to_jsonl(&entries).unwrap();
    let parsed = parse_jsonl(&jsonl).unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].name, "payment");
    assert_eq!(parsed[1].pinned, true);
}
