//! Publication-bound graph reporting projections.
//!
//! Registry metadata is an optimization only when every publication identity
//! field is present and the report's content version matches exactly. Callers
//! retain their live-query fallback for legacy or mismatched entries.

use cih_core::{RegistryEntry, RegistryGraphReport};
use cih_graph_store::{GraphOverview, GraphOverviewNode, GraphSummary, KindCount};

pub(crate) fn current(entry: &RegistryEntry) -> Option<&RegistryGraphReport> {
    let content_version = entry.published_graph_content_version.as_deref()?;
    let report = entry.stats.published_graph_report.as_ref()?;
    (entry.published_artifact_version.is_some()
        && entry.published_epoch.is_some()
        && report.matches_content(content_version)
        && report.kinds.iter().map(|kind| kind.count).sum::<u64>() == report.total_nodes)
        .then_some(report)
}

pub(crate) fn summary(report: &RegistryGraphReport) -> GraphSummary {
    GraphSummary {
        kinds: report
            .kinds
            .iter()
            .map(|kind| KindCount {
                kind: kind.kind.clone(),
                count: kind.count,
            })
            .collect(),
        total_nodes: report.total_nodes,
        total_edges: report.total_edges,
    }
}

pub(crate) fn symbol_pool(report: &RegistryGraphReport) -> GraphOverview {
    GraphOverview {
        nodes: report
            .symbol_hubs
            .iter()
            .map(|hub| GraphOverviewNode {
                node: hub.node.clone(),
                degree: hub.degree,
            })
            .collect(),
        edges: Vec::new(),
        total_nodes: report.total_nodes,
        total_edges: report.total_edges,
        truncated: report.symbol_hubs.len() < report.total_nodes as usize || report.total_edges > 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{RegistryKindCount, RegistryStats};

    fn entry(report_version: Option<&str>) -> RegistryEntry {
        let mut stats = RegistryStats::default();
        stats.published_graph_report = report_version.map(|version| RegistryGraphReport {
            schema_version: 1,
            graph_content_version: version.to_string(),
            total_nodes: 2,
            total_edges: 1,
            kinds: vec![RegistryKindCount {
                kind: "Method".into(),
                count: 2,
            }],
            symbol_hubs: Vec::new(),
        });
        RegistryEntry {
            repository_id: None,
            name: "demo".into(),
            path: "/repo".into(),
            graph_key: "demo".into(),
            artifacts_dir: "/repo/.cih/artifacts/base-v1".into(),
            latest_artifact_version: Some("base-v1".into()),
            published_artifact_version: Some("base-v1".into()),
            published_graph_content_version: Some("content-v1".into()),
            published_epoch: Some("epoch-v1".into()),
            community_artifacts_dir: None,
            indexed_at: "2026-07-26T00:00:00Z".into(),
            last_git_head: None,
            stats,
        }
    }

    #[test]
    fn current_requires_exact_publication_binding() {
        assert!(current(&entry(None)).is_none());
        assert!(current(&entry(Some("content-v0"))).is_none());
        let entry = entry(Some("content-v1"));
        let report = current(&entry).expect("matching publication report");
        let summary = summary(report);
        assert_eq!(summary.total_nodes, 2);
        assert_eq!(summary.total_edges, 1);
        assert_eq!(summary.kinds[0].kind, "Method");
    }

    #[test]
    fn current_rejects_internally_inconsistent_kind_totals() {
        let mut entry = entry(Some("content-v1"));
        entry
            .stats
            .published_graph_report
            .as_mut()
            .expect("report")
            .kinds[0]
            .count = 1;
        assert!(current(&entry).is_none());
    }
}
