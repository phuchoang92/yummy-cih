//! LLM-free assembly of the class-derived enrichment maps (P3.8 C2).
//!
//! The batch enrichment (`cih-engine` `enrich_classes_for_chains`) fills a
//! [`ClassEnrichmentStore`] via the LLM, then aggregates its per-class entries
//! into the per-controller / per-community summary maps that `WikiInput`
//! carries. That aggregation — and the graph traversal that decides which
//! classes to enrich — is pure. Factoring it here lets both the batch (after it
//! fills the store) and the **read-only** live-serving path (which just loads
//! the persisted `class-enrichment.json`) build identical maps.

use std::collections::{BTreeMap, HashMap};

use crate::graph::route_path;
use crate::{ClassEnrichmentStore, CommunityLlmSummary, ControllerLlmSummary, WikiGraph};

/// The class → method-chain map: for every controller route handler, the FQCNs
/// reachable within a bounded call chain, and the method ids per class. This is
/// the set of classes enrichment covers (LLM-free — pure graph traversal).
///
/// `filter_route` (empty = no filter) restricts to routes whose path contains a
/// filter substring, matching the batch.
pub fn class_method_chains(
    graph: &WikiGraph,
    filter_route: &[String],
) -> BTreeMap<String, Vec<String>> {
    let mut class_methods: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for routes in graph.routes_by_controller.values() {
        for (handler, route) in routes {
            if !filter_route.is_empty() && {
                let path = route_path(route);
                !filter_route.iter().any(|f| path.contains(f.as_str()))
            } {
                continue;
            }
            let chain = graph.build_call_chain(handler.id.as_str(), 4);
            for method_id in chain {
                let fqcn = method_id
                    .strip_prefix("Method:")
                    .or_else(|| method_id.strip_prefix("Constructor:"))
                    .and_then(|s| s.split('#').next())
                    .unwrap_or("")
                    .to_string();
                if fqcn.is_empty() {
                    continue;
                }
                let methods = class_methods.entry(fqcn).or_default();
                if !methods.contains(&method_id) {
                    methods.push(method_id);
                }
            }
        }
    }
    class_methods
}

/// Aggregate a filled [`ClassEnrichmentStore`] into the per-controller and
/// per-community summary maps (`controller_summaries` + `llm_summaries` on
/// `WikiInput`). Pure: classes absent from the store are simply skipped, so a
/// partially-populated cache degrades gracefully (missing classes render
/// graph-only).
pub fn build_class_maps(
    graph: &WikiGraph,
    class_methods: &BTreeMap<String, Vec<String>>,
    store: &ClassEnrichmentStore,
) -> (
    HashMap<String, ControllerLlmSummary>,
    HashMap<String, CommunityLlmSummary>,
) {
    let entries = &store.entries;

    let mut ctrl_map: HashMap<String, ControllerLlmSummary> = HashMap::new();
    for fqcn in class_methods.keys() {
        let simple_name = fqcn.rsplit('.').next().unwrap_or(fqcn.as_str()).to_string();
        if let Some(entry) = entries.get(fqcn.as_str()) {
            ctrl_map.insert(
                simple_name,
                ControllerLlmSummary {
                    description: entry.class_summary.clone(),
                    feature: None,
                    method_descriptions: entry.method_descriptions.clone(),
                },
            );
        }
    }

    let mut comm_texts: HashMap<String, Vec<String>> = HashMap::new();
    for (fqcn, method_ids) in class_methods {
        let Some(entry) = entries.get(fqcn.as_str()) else {
            continue;
        };
        if entry.class_summary.is_empty() {
            continue;
        }
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for mid in method_ids {
            if let Some(comm_id) = graph.community_by_member.get(mid.as_str()) {
                if seen.insert(comm_id.as_str()) {
                    comm_texts
                        .entry(comm_id.clone())
                        .or_default()
                        .push(entry.class_summary.clone());
                }
            }
        }
    }
    let comm_map: HashMap<String, CommunityLlmSummary> = comm_texts
        .into_iter()
        .map(|(id, summaries)| {
            let text = summaries.join(" ");
            (
                id,
                CommunityLlmSummary {
                    po: text.clone(),
                    ba: text,
                    dev: String::new(),
                },
            )
        })
        .collect();

    (ctrl_map, comm_map)
}
