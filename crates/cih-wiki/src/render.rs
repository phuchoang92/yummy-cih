//! Render context: all derived state needed to render any wiki page, computed
//! once from `(graph, input, feature_groups)`. Promoted from the former
//! `PageGenCtx` so per-page rendering can happen outside the batch pipeline
//! (P2.5a — prerequisite for on-demand serving, P3.8).
//!
//! The batch `generate_wiki` and (future) single-page `render_page` both read
//! from the same `RenderContext`, so their output is identical by construction.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use cih_core::NodeKind;

use crate::features::{self, FeatureGroup};
use crate::graph::WikiGraph;
use crate::{clean_method_desc, WikiInput};

/// All derived state for one feature's pages, precomputed so a single feature
/// page (dev, api-flow) can be rendered without running the whole feature loop.
pub(crate) struct FeatureContext {
    /// Community ids grouped into this feature.
    pub community_ids: Vec<String>,
    /// Classes whose dev pages belong to this feature (primary-feature deduped).
    pub class_set: BTreeSet<String>,
    /// class_id → dev page slug, for every class in `class_set`.
    pub slug_for: HashMap<String, String>,
    /// (class_name, "dev/{slug}") sorted by class name — the feature index table.
    pub class_dev_links: Vec<(String, String)>,
    /// Controller class names owned by this feature (effective-feature resolved),
    /// sorted. Routes are looked up from the graph at render time.
    pub controllers: Vec<String>,
}

/// Immutable render state shared by the batch pipeline and single-page renders.
pub struct RenderContext<'a> {
    pub input: &'a WikiInput<'a>,
    // `feature_order`, `class_primary_feature`, and `known_features` are read by
    // the standalone slug resolver / `render_page` (added in a follow-up commit);
    // held here now so the batch and single-page paths share one construction.
    /// Feature names in batch-emission order (`feature_groups` order).
    #[allow(dead_code)]
    pub(crate) feature_order: Vec<String>,
    pub(crate) features: BTreeMap<String, FeatureContext>,
    pub(crate) method_flow_desc: HashMap<String, String>,
    #[allow(dead_code)]
    pub(crate) class_primary_feature: HashMap<String, String>,
    pub(crate) process_by_handler: HashMap<String, String>,
    pub(crate) feature_scheduled_counts: HashMap<String, usize>,
    pub(crate) feature_listener_counts: HashMap<String, usize>,
    #[allow(dead_code)]
    pub(crate) known_features: HashSet<String>,
    pub(crate) enrichment_tier: &'static str,
}

impl<'a> RenderContext<'a> {
    pub fn build(
        graph: &WikiGraph,
        input: &'a WikiInput<'a>,
        feature_groups: &[FeatureGroup],
    ) -> Self {
        let feature_order: Vec<String> =
            feature_groups.iter().map(|g| g.feature.clone()).collect();
        let known_features: HashSet<String> = feature_order.iter().cloned().collect();

        let method_flow_desc = build_method_flow_desc(graph, input);
        let class_primary_feature = build_class_primary_feature(graph, feature_groups);
        let process_by_handler: HashMap<String, String> = graph
            .process_steps
            .iter()
            .filter_map(|(proc_id, steps)| {
                steps
                    .first()
                    .map(|s| (s.symbol.id.as_str().to_string(), proc_id.clone()))
            })
            .collect();
        let (feature_scheduled_counts, feature_listener_counts) =
            build_entrypoint_counts(graph, input);

        let enrichment_tier = if input.llm_full.is_some() {
            "llm-full"
        } else if input.llm_summaries.is_some() || input.feature_llm_summaries.is_some() {
            "llm-summary"
        } else {
            "graph-only"
        };

        let mut features = BTreeMap::new();
        for group in feature_groups {
            let fctx =
                build_feature_context(graph, input, group, &class_primary_feature, &known_features);
            features.insert(group.feature.clone(), fctx);
        }

        Self {
            input,
            feature_order,
            features,
            method_flow_desc,
            class_primary_feature,
            process_by_handler,
            feature_scheduled_counts,
            feature_listener_counts,
            known_features,
            enrichment_tier,
        }
    }

    pub(crate) fn feature(&self, feature: &str) -> Option<&FeatureContext> {
        self.features.get(feature)
    }
}

/// Resolve a member's owning class id, matching the batch's class-id derivation
/// exactly: strip the `Method:`/`Constructor:`/`Function:` prefix, then prefer a
/// `Class:`/`Interface:`/`Enum:`/`Record:` id that exists in the graph, else
/// fall back to `Class:{fqcn}`.
pub(crate) fn resolve_member_class_id(graph: &WikiGraph, member_id: &str) -> Option<String> {
    member_id.split_once('#').map(|(prefix, _)| {
        let fqcn = prefix
            .trim_start_matches("Method:")
            .trim_start_matches("Constructor:")
            .trim_start_matches("Function:");
        ["Class:", "Interface:", "Enum:", "Record:"]
            .iter()
            .map(|pfx| format!("{}{}", pfx, fqcn))
            .find(|id| {
                graph.nodes_by_id.contains_key(id.as_str())
                    || graph.methods_by_class.contains_key(id.as_str())
            })
            .unwrap_or_else(|| format!("Class:{}", fqcn))
    })
}

/// A class's display name, matching the batch fallback (short name from the id).
fn class_display_name(graph: &WikiGraph, id: &str) -> String {
    graph
        .nodes_by_id
        .get(id)
        .map(|n| n.name.clone())
        .unwrap_or_else(|| {
            id.trim_start_matches("Class:")
                .rsplit('.')
                .next()
                .unwrap_or("Unknown")
                .to_string()
        })
}

fn build_feature_context(
    graph: &WikiGraph,
    input: &WikiInput<'_>,
    group: &FeatureGroup,
    class_primary_feature: &HashMap<String, String>,
    known_features: &HashSet<String>,
) -> FeatureContext {
    let feature = &group.feature;
    let cids = &group.community_ids;

    let mut class_set: BTreeSet<String> = BTreeSet::new();
    for comm_id in cids {
        if let Some(members) = graph.members_by_community.get(comm_id) {
            for m in members {
                if !matches!(
                    m.kind,
                    NodeKind::Method | NodeKind::Function | NodeKind::Constructor
                ) {
                    continue;
                }
                if let Some(cls_id) = resolve_member_class_id(graph, m.id.as_str()) {
                    if class_primary_feature
                        .get(&cls_id)
                        .map(|f| f == feature)
                        .unwrap_or(true)
                    {
                        class_set.insert(cls_id);
                    }
                }
            }
        }
    }

    let slug_for = features::assign_class_slugs(&class_set, |id| class_display_name(graph, id));

    let mut class_dev_links: Vec<(String, String)> = class_set
        .iter()
        .map(|id| {
            let name = class_display_name(graph, id.as_str());
            let slug = slug_for
                .get(id.as_str())
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            (name, format!("dev/{}", slug))
        })
        .collect();
    class_dev_links.sort_by(|a, b| a.0.cmp(&b.0));

    let mut controllers: Vec<String> = graph
        .routes_by_controller
        .keys()
        .filter(|ctrl| {
            effective_controller_feature(graph, input, ctrl, known_features) == feature.as_str()
        })
        .cloned()
        .collect();
    controllers.sort();

    FeatureContext {
        community_ids: cids.clone(),
        class_set,
        slug_for,
        class_dev_links,
        controllers,
    }
}

/// The feature a controller belongs to, applying the LLM-suggested feature only
/// when the file-path heuristic returned "shared" and the suggestion is a real
/// feature. Matches the batch filter in `emit_feature_section`.
fn effective_controller_feature<'g>(
    graph: &'g WikiGraph,
    input: &'g WikiInput<'_>,
    ctrl: &str,
    known_features: &HashSet<String>,
) -> &'g str {
    let graph_feature = graph
        .controller_feature
        .get(ctrl)
        .map(|f| f.as_str())
        .unwrap_or("shared");
    if graph_feature == "shared" {
        let llm_feat = input
            .controller_summaries
            .as_ref()
            .and_then(|m| m.get(ctrl))
            .and_then(|s| s.feature.as_deref())
            .unwrap_or("shared");
        if known_features.contains(llm_feat) {
            llm_feat
        } else {
            graph_feature
        }
    } else {
        graph_feature
    }
}

/// Flat method_id → description lookup: from flow summaries (per step) merged
/// with controller per-method descriptions. Cut-paste of the batch builder.
fn build_method_flow_desc(graph: &WikiGraph, input: &WikiInput<'_>) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    if let Some(flow_map) = &input.flow_llm_summaries {
        for (proc_id, summary) in flow_map {
            if let Some(steps) = graph.process_steps.get(proc_id.as_str()) {
                for step in steps {
                    let idx = step.step_number.saturating_sub(1);
                    if let Some(desc) = summary.step_descriptions.get(idx) {
                        if !desc.is_empty() {
                            let id = step.symbol.id.as_str();
                            let cleaned = if let Some((prefix, meth_arity)) = id.split_once('#') {
                                let cls = prefix
                                    .trim_start_matches("Method:")
                                    .trim_start_matches("Constructor:")
                                    .rsplit('.')
                                    .next()
                                    .unwrap_or("");
                                let meth = meth_arity.split('/').next().unwrap_or("");
                                clean_method_desc(desc, cls, meth)
                            } else {
                                desc.clone()
                            };
                            map.insert(id.to_string(), cleaned);
                        }
                    }
                }
            }
        }
    }

    if let Some(ctrl_summaries) = &input.controller_summaries {
        for method_nodes in graph.methods_by_class.values() {
            for method in method_nodes {
                let method_id = method.id.as_str();
                let class_name = method_id.split_once('#').and_then(|(prefix, _)| {
                    prefix
                        .trim_start_matches("Method:")
                        .trim_start_matches("Constructor:")
                        .rsplit('.')
                        .next()
                });
                let simple_method_name =
                    method_id.split('#').nth(1).and_then(|x| x.split('/').next());
                if let (Some(cls), Some(meth)) = (class_name, simple_method_name) {
                    if let Some(ctrl_summary) = ctrl_summaries.get(cls) {
                        if let Some(desc) = ctrl_summary.method_descriptions.get(meth) {
                            map.entry(method_id.to_string())
                                .or_insert_with(|| clean_method_desc(desc, cls, meth));
                        }
                    }
                }
            }
        }
    }
    map
}

/// class_id → primary feature (the feature contributing the most member methods).
/// Cut-paste of the batch pre-pass so a class only owns one dev page.
fn build_class_primary_feature(
    graph: &WikiGraph,
    feature_groups: &[FeatureGroup],
) -> HashMap<String, String> {
    let mut votes: HashMap<String, BTreeMap<String, usize>> = HashMap::new();
    for group in feature_groups {
        for comm_id in &group.community_ids {
            if let Some(members) = graph.members_by_community.get(comm_id) {
                for m in members {
                    if !matches!(
                        m.kind,
                        NodeKind::Method | NodeKind::Function | NodeKind::Constructor
                    ) {
                        continue;
                    }
                    if let Some(cls_id) = resolve_member_class_id(graph, m.id.as_str()) {
                        *votes
                            .entry(cls_id)
                            .or_default()
                            .entry(group.feature.clone())
                            .or_insert(0) += 1;
                    }
                }
            }
        }
    }
    votes
        .into_iter()
        .map(|(cls_id, feature_votes)| {
            let best = feature_votes
                .into_iter()
                .max_by_key(|(_, v)| *v)
                .map(|(f, _)| f)
                .unwrap_or_default();
            (cls_id, best)
        })
        .collect()
}

/// Per-feature scheduled / event-listener counts for the PO page API-surface
/// table. Cut-paste of the batch builder.
fn build_entrypoint_counts(
    graph: &WikiGraph,
    input: &WikiInput<'_>,
) -> (HashMap<String, usize>, HashMap<String, usize>) {
    let mut scheduled: HashMap<String, usize> = HashMap::new();
    let mut listeners: HashMap<String, usize> = HashMap::new();
    for ep in &input.entrypoints {
        let file = graph
            .nodes_by_id
            .get(ep.method_id.as_str())
            .map(|n| n.file.as_str())
            .unwrap_or("");
        let feature = (input.feature_of)(ep.method_id.as_str(), file);
        match ep.kind.as_str() {
            "scheduled" => *scheduled.entry(feature).or_insert(0) += 1,
            "event_listener" => *listeners.entry(feature).or_insert(0) += 1,
            _ => {}
        }
    }
    (scheduled, listeners)
}
