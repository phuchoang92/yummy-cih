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
use crate::{clean_method_desc, EntrypointRecord, WikiInput};

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
    /// Feature groups (post filter/synthesis) in batch-emission order.
    pub(crate) feature_groups: Vec<FeatureGroup>,
    /// Feature names in `feature_groups` order (for `dev_slugs_visible`).
    pub(crate) feature_order: Vec<String>,
    pub(crate) features: BTreeMap<String, FeatureContext>,
    pub(crate) method_flow_desc: HashMap<String, String>,
    pub(crate) process_by_handler: HashMap<String, String>,
    pub(crate) feature_scheduled_counts: HashMap<String, usize>,
    pub(crate) feature_listener_counts: HashMap<String, usize>,
    /// Simple-method-name → description, from every controller summary. Feeds
    /// the entrypoint (scheduled/listener) flow pages.
    pub(crate) all_method_desc: HashMap<String, String>,
    /// community_id → directory slug.
    pub(crate) comm_slug_map: BTreeMap<String, String>,
    /// Scheduled / event-listener entrypoints grouped by feature (batch order).
    pub(crate) scheduled_by_feature: BTreeMap<String, Vec<EntrypointRecord>>,
    pub(crate) listeners_by_feature: BTreeMap<String, Vec<EntrypointRecord>>,
    pub(crate) enrichment_tier: &'static str,
}

impl<'a> RenderContext<'a> {
    pub fn build(
        graph: &WikiGraph,
        input: &'a WikiInput<'a>,
        feature_groups: &[FeatureGroup],
    ) -> Self {
        let feature_order: Vec<String> = feature_groups.iter().map(|g| g.feature.clone()).collect();
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

        let all_method_desc: HashMap<String, String> = input
            .controller_summaries
            .iter()
            .flat_map(|m| m.values())
            .flat_map(|s| s.method_descriptions.iter())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let comm_slug_map = crate::slugify::build_slug_map(&graph.community_nodes);
        let (scheduled_by_feature, listeners_by_feature) =
            build_entrypoints_by_feature(graph, input);

        Self {
            input,
            feature_groups: feature_groups.to_vec(),
            feature_order,
            features,
            method_flow_desc,
            process_by_handler,
            feature_scheduled_counts,
            feature_listener_counts,
            all_method_desc,
            comm_slug_map,
            scheduled_by_feature,
            listeners_by_feature,
            enrichment_tier,
        }
    }

    pub(crate) fn feature(&self, feature: &str) -> Option<&FeatureContext> {
        self.features.get(feature)
    }

    /// The dev-page slugs visible when rendering pages of `upto` (inclusive) in
    /// feature order — replicating the batch's incremental `class_dev_slugs`
    /// accumulation exactly. `upto = None` ⇒ all features (entrypoint pages,
    /// rendered after the whole feature loop). `rendered` restricts to the
    /// `--since` affected set. Consumers do point lookups only, so identical
    /// map CONTENTS (not order) suffice for byte-identity.
    pub fn dev_slugs_visible(
        &self,
        upto: Option<&str>,
        rendered: Option<&HashSet<String>>,
    ) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for f in &self.feature_order {
            if let Some(r) = rendered {
                if !r.contains(f) {
                    if upto == Some(f.as_str()) {
                        break;
                    }
                    continue;
                }
            }
            if let Some(fctx) = self.features.get(f) {
                for (cid, slug) in &fctx.slug_for {
                    out.insert(cid.clone(), slug.clone());
                }
            }
            if upto == Some(f.as_str()) {
                break;
            }
        }
        out
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
                let simple_method_name = method_id
                    .split('#')
                    .nth(1)
                    .and_then(|x| x.split('/').next());
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

/// Scheduled / event-listener entrypoints grouped by feature, in the same
/// `BTreeMap` order the batch emits. Records are cloned so the context owns them.
#[allow(clippy::type_complexity)]
fn build_entrypoints_by_feature(
    graph: &WikiGraph,
    input: &WikiInput<'_>,
) -> (
    BTreeMap<String, Vec<EntrypointRecord>>,
    BTreeMap<String, Vec<EntrypointRecord>>,
) {
    let mut scheduled: BTreeMap<String, Vec<EntrypointRecord>> = BTreeMap::new();
    let mut listeners: BTreeMap<String, Vec<EntrypointRecord>> = BTreeMap::new();
    for ep in &input.entrypoints {
        let file = graph
            .nodes_by_id
            .get(ep.method_id.as_str())
            .map(|n| n.file.as_str())
            .unwrap_or("");
        let feature = (input.feature_of)(ep.method_id.as_str(), file);
        match ep.kind.as_str() {
            "scheduled" => scheduled.entry(feature).or_default().push(ep.clone()),
            "event_listener" => listeners.entry(feature).or_default().push(ep.clone()),
            _ => {}
        }
    }
    (scheduled, listeners)
}

// ── Standalone per-page rendering (P2.5a) ────────────────────────────────────

use anyhow::Result;
use cih_core::{Node, NodeId, Range};

use crate::manifest::{NavEntry, PageEntry};

/// One rendered wiki page plus everything the batch pipeline registers for it,
/// so a single-page render yields the same manifest/nav/agent-index data.
pub struct RenderedPage {
    /// Primary markdown path relative to `out_dir`, e.g. `pages/cart/po.md`.
    pub rel_path: String,
    pub content: String,
    /// JSON sidecar (dev-class + routes pages): `(rel_path, content)`.
    pub json: Option<(String, String)>,
    /// Manifest registration; `None` for pages absent from the manifest
    /// (controller API index pages).
    pub entry: Option<PageEntry>,
    /// Nav registration: `(nav key, entry)`.
    pub nav: Vec<(String, NavEntry)>,
    /// agent-index contribution for dev pages: `(class_node_id, source_file)`.
    pub agent_index: Option<(String, String)>,
}

/// Identity of a single wiki page, resolvable from its manifest slug.
#[derive(Clone, Debug)]
pub enum PageSubject {
    SystemIndex,
    Routes,
    FeatureIndex {
        feature: String,
    },
    FeaturePo {
        feature: String,
    },
    FeatureBa {
        feature: String,
    },
    DevClass {
        feature: String,
        class_id: String,
    },
    ControllerIndex {
        feature: String,
        controller: String,
    },
    ApiFlow {
        feature: String,
        controller: String,
        handler_id: String,
        position: usize,
    },
    ScheduledFlow {
        feature: String,
        method_id: String,
        position: usize,
    },
    ListenerFlow {
        feature: String,
        method_id: String,
        topics: Vec<String>,
        position: usize,
    },
    CommunityIndex,
    CommunityDetail {
        community_id: String,
    },
    CommunityPo {
        community_id: String,
    },
    CommunityBa {
        community_id: String,
    },
}

/// Slug → subject index in exact batch-emission order. Built by walking the
/// same enumeration the batch loops use, so a single-page render and the batch
/// address the same page set.
pub struct PageIndex {
    ordered: Vec<PageSubject>,
    by_slug: BTreeMap<String, PageSubject>,
}

impl PageIndex {
    /// Subjects in batch-emission order.
    pub fn subjects(&self) -> &[PageSubject] {
        &self.ordered
    }

    /// All addressable page slugs.
    pub fn slugs(&self) -> impl Iterator<Item = &str> {
        self.by_slug.keys().map(String::as_str)
    }
}

/// Resolve a manifest slug to its page subject.
pub fn resolve_slug<'i>(index: &'i PageIndex, slug: &str) -> Option<&'i PageSubject> {
    index.by_slug.get(slug)
}

/// Build the page index by enumerating every page in the batch's order.
pub fn build_page_index(graph: &WikiGraph, ctx: &RenderContext<'_>) -> PageIndex {
    let mut ordered: Vec<PageSubject> = Vec::new();
    let mut by_slug: BTreeMap<String, PageSubject> = BTreeMap::new();
    let mut push = |slug: String, subject: PageSubject| {
        by_slug.insert(slug, subject.clone());
        ordered.push(subject);
    };

    push("index".into(), PageSubject::SystemIndex);
    push("routes".into(), PageSubject::Routes);

    for group in &ctx.feature_groups {
        let feature = &group.feature;
        let Some(fctx) = ctx.features.get(feature) else {
            continue;
        };
        push(
            format!("{}/index", feature),
            PageSubject::FeatureIndex {
                feature: feature.clone(),
            },
        );
        push(
            format!("{}/po", feature),
            PageSubject::FeaturePo {
                feature: feature.clone(),
            },
        );
        push(
            format!("{}/ba", feature),
            PageSubject::FeatureBa {
                feature: feature.clone(),
            },
        );
        for class_id in &fctx.class_set {
            let slug = fctx
                .slug_for
                .get(class_id.as_str())
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            push(
                format!("{}/dev/{}", feature, slug),
                PageSubject::DevClass {
                    feature: feature.clone(),
                    class_id: class_id.clone(),
                },
            );
        }
        for controller in &fctx.controllers {
            let Some(routes) = graph.routes_by_controller.get(controller) else {
                continue;
            };
            let ctrl_slug = crate::slugify::slugify(controller);
            push(
                format!("{}/api/{}/index", feature, ctrl_slug),
                PageSubject::ControllerIndex {
                    feature: feature.clone(),
                    controller: controller.clone(),
                },
            );
            for (route_pos, (handler, _route)) in routes.iter().enumerate() {
                let handler_slug = crate::pages::api_flow::handler_slug(handler.id.as_str());
                push(
                    format!("{}/api/{}/{}", feature, ctrl_slug, handler_slug),
                    PageSubject::ApiFlow {
                        feature: feature.clone(),
                        controller: controller.clone(),
                        handler_id: handler.id.as_str().to_string(),
                        position: route_pos + 1,
                    },
                );
            }
        }
    }

    for (feature, entries) in &ctx.scheduled_by_feature {
        for (pos, ep) in entries.iter().enumerate() {
            let slug = crate::pages::api_flow::handler_slug(ep.method_id.as_str());
            push(
                format!("{}/api/scheduled/{}", feature, slug),
                PageSubject::ScheduledFlow {
                    feature: feature.clone(),
                    method_id: ep.method_id.clone(),
                    position: pos + 1,
                },
            );
        }
    }
    for (feature, entries) in &ctx.listeners_by_feature {
        for (pos, ep) in entries.iter().enumerate() {
            let slug = crate::pages::api_flow::handler_slug(ep.method_id.as_str());
            push(
                format!("{}/api/events/{}", feature, slug),
                PageSubject::ListenerFlow {
                    feature: feature.clone(),
                    method_id: ep.method_id.clone(),
                    topics: ep.topics.clone(),
                    position: pos + 1,
                },
            );
        }
    }

    push("communities/index".into(), PageSubject::CommunityIndex);
    for comm in &graph.community_nodes {
        let comm_id = comm.id.as_str().to_string();
        let dir_name = ctx
            .comm_slug_map
            .get(&comm_id)
            .cloned()
            .unwrap_or_else(|| crate::slugify::slugify(comm.id.as_str()));
        push(
            format!("communities/{dir_name}/index"),
            PageSubject::CommunityDetail {
                community_id: comm_id.clone(),
            },
        );
        if ctx
            .input
            .llm_full
            .as_ref()
            .and_then(|m| m.get(&comm_id))
            .is_some()
        {
            push(
                format!("communities/{dir_name}/po"),
                PageSubject::CommunityPo {
                    community_id: comm_id.clone(),
                },
            );
            push(
                format!("communities/{dir_name}/ba"),
                PageSubject::CommunityBa {
                    community_id: comm_id.clone(),
                },
            );
        }
    }

    PageIndex { ordered, by_slug }
}

/// Render a single wiki page by manifest slug, outside the batch pipeline.
/// Returns `None` for an unknown slug (or a slug whose subject can't render).
/// The `rendered` filter mirrors `--since`: `None` for a full render.
pub fn render_page(
    graph: &WikiGraph,
    ctx: &RenderContext<'_>,
    index: &PageIndex,
    slug: &str,
    rendered: Option<&HashSet<String>>,
) -> Option<RenderedPage> {
    let subject = resolve_slug(index, slug)?;
    render_subject(graph, ctx, subject, rendered).ok()
}

fn page_meta<'m>(ctx: &'m RenderContext<'_>) -> crate::pages::WikiPageMeta<'m> {
    crate::pages::WikiPageMeta {
        enrichment_tier: ctx.enrichment_tier,
        graph_version: &ctx.input.graph_version,
    }
}

/// The single-page render core shared by `render_page`. Reproduces the exact
/// content, sidecars, and manifest/nav/agent-index registration the batch emits
/// for one page.
pub(crate) fn render_subject(
    graph: &WikiGraph,
    ctx: &RenderContext<'_>,
    subject: &PageSubject,
    rendered: Option<&HashSet<String>>,
) -> Result<RenderedPage> {
    use crate::pages;
    let input = ctx.input;
    match subject {
        PageSubject::SystemIndex => {
            let content = pages::system_index::render_system_index(
                &ctx.feature_groups,
                graph,
                &input.repo_name,
            );
            Ok(RenderedPage {
                rel_path: "pages/index.md".into(),
                content,
                json: None,
                entry: Some(PageEntry {
                    slug: "index".into(),
                    role: "system".into(),
                    title: input.repo_name.clone(),
                    kind: "index".into(),
                    path: "pages/index.md".into(),
                    json_path: None,
                    community_id: None,
                }),
                nav: Vec::new(),
                agent_index: None,
            })
        }
        PageSubject::Routes => {
            let content = pages::shared::render_routes_page(graph);
            let json_val = pages::shared::render_routes_json(graph);
            Ok(RenderedPage {
                rel_path: "pages/routes.md".into(),
                content,
                json: Some((
                    "pages/routes.json".into(),
                    serde_json::to_string_pretty(&json_val)?,
                )),
                entry: Some(PageEntry {
                    slug: "routes".into(),
                    role: "shared".into(),
                    title: "API Routes".into(),
                    kind: "routes".into(),
                    path: "pages/routes.md".into(),
                    json_path: Some("pages/routes.json".into()),
                    community_id: None,
                }),
                nav: Vec::new(),
                agent_index: None,
            })
        }
        PageSubject::FeatureIndex { feature } => {
            let fctx = ctx.feature(feature).expect("feature context");
            let meta = page_meta(ctx);
            let content = pages::feature_index::render_feature_index(
                feature,
                &fctx.community_ids,
                &fctx.class_dev_links,
                graph,
                &meta,
            );
            let title = format!("{} Overview", crate::capitalize(feature));
            Ok(RenderedPage {
                rel_path: format!("pages/{}/index.md", feature),
                content,
                json: None,
                entry: Some(PageEntry {
                    slug: format!("{}/index", feature),
                    role: feature.clone(),
                    title: title.clone(),
                    kind: "index".into(),
                    path: format!("pages/{}/index.md", feature),
                    json_path: None,
                    community_id: None,
                }),
                nav: vec![(
                    feature.clone(),
                    NavEntry {
                        slug: format!("{}/index", feature),
                        title,
                        kind: "index".into(),
                    },
                )],
                agent_index: None,
            })
        }
        PageSubject::FeaturePo { feature } => {
            let fctx = ctx.feature(feature).expect("feature context");
            let meta = page_meta(ctx);
            let feature_llm = input
                .feature_llm_summaries
                .as_ref()
                .and_then(|m| m.get(feature.as_str()));
            let content = pages::feature_po::render_feature_po(
                feature,
                &fctx.community_ids,
                graph,
                input.llm_summaries.as_ref(),
                input.llm_full.as_ref(),
                feature_llm,
                input.flow_llm_summaries.as_ref(),
                ctx.feature_scheduled_counts
                    .get(feature.as_str())
                    .copied()
                    .unwrap_or(0),
                ctx.feature_listener_counts
                    .get(feature.as_str())
                    .copied()
                    .unwrap_or(0),
                &meta,
            );
            let title = format!("{} — Business Overview", crate::capitalize(feature));
            Ok(feature_page(feature, "po", title, content))
        }
        PageSubject::FeatureBa { feature } => {
            let fctx = ctx.feature(feature).expect("feature context");
            let meta = page_meta(ctx);
            let feature_llm = input
                .feature_llm_summaries
                .as_ref()
                .and_then(|m| m.get(feature.as_str()));
            let content = pages::feature_ba::render_feature_ba(
                feature,
                &fctx.community_ids,
                graph,
                input.llm_summaries.as_ref(),
                input.llm_full.as_ref(),
                feature_llm,
                input.flow_llm_summaries.as_ref(),
                &meta,
            );
            let title = format!("{} — Business Analysis", crate::capitalize(feature));
            Ok(feature_page(feature, "ba", title, content))
        }
        PageSubject::DevClass { feature, class_id } => {
            let fctx = ctx.feature(feature).expect("feature context");
            let meta = page_meta(ctx);
            let slug = fctx
                .slug_for
                .get(class_id.as_str())
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            let page_path = format!("{}/dev/{}", feature, slug);
            let synthesized;
            let cls_node: &Node = match graph.nodes_by_id.get(class_id.as_str()) {
                Some(n) => n,
                None => {
                    let simple_name = class_id
                        .trim_start_matches("Class:")
                        .rsplit('.')
                        .next()
                        .unwrap_or("Unknown")
                        .to_string();
                    let file = graph
                        .methods_by_class
                        .get(class_id.as_str())
                        .and_then(|ms| ms.first())
                        .map(|m| m.file.clone())
                        .unwrap_or_default();
                    synthesized = Node {
                        id: NodeId::new(class_id.clone()),
                        kind: cih_core::NodeKind::Class,
                        name: simple_name,
                        qualified_name: None,
                        file,
                        range: Range::default(),
                        props: None,
                    };
                    &synthesized
                }
            };
            let content = pages::dev::render_dev_class(
                graph,
                cls_node,
                &input.bodies,
                &ctx.method_flow_desc,
                &meta,
            );
            let json_val = pages::dev::render_dev_class_json(graph, cls_node);
            let dev_title = cls_node.name.clone();
            Ok(RenderedPage {
                rel_path: format!("pages/{}.md", page_path),
                content,
                json: Some((
                    format!("pages/{}.json", page_path),
                    serde_json::to_string_pretty(&json_val)?,
                )),
                entry: Some(PageEntry {
                    slug: page_path.clone(),
                    role: feature.clone(),
                    title: dev_title.clone(),
                    kind: "dev".into(),
                    path: format!("pages/{}.md", page_path),
                    json_path: Some(format!("pages/{}.json", page_path)),
                    community_id: None,
                }),
                nav: vec![(
                    feature.clone(),
                    NavEntry {
                        slug: page_path.clone(),
                        title: dev_title,
                        kind: "dev".into(),
                    },
                )],
                agent_index: Some((class_id.clone(), cls_node.file.clone())),
            })
        }
        PageSubject::ControllerIndex {
            feature,
            controller,
        } => {
            let routes = graph
                .routes_by_controller
                .get(controller)
                .ok_or_else(|| anyhow::anyhow!("controller routes missing: {controller}"))?;
            let ctrl_slug = crate::slugify::slugify(controller);
            let ctrl_summary = input
                .controller_summaries
                .as_ref()
                .and_then(|m| m.get(controller));
            let description = ctrl_summary
                .map(|s| s.description.as_str())
                .filter(|s| !s.is_empty());
            let empty_methods = HashMap::new();
            let method_descriptions = ctrl_summary
                .map(|s| &s.method_descriptions)
                .unwrap_or(&empty_methods);
            let content = pages::feature_po::render_controller_page(
                controller,
                routes,
                description,
                method_descriptions,
            );
            Ok(RenderedPage {
                rel_path: format!("pages/{}/api/{}/index.md", feature, ctrl_slug),
                content,
                json: None,
                entry: None,
                nav: Vec::new(),
                agent_index: None,
            })
        }
        PageSubject::ApiFlow {
            feature,
            controller,
            handler_id,
            position,
        } => {
            let routes = graph
                .routes_by_controller
                .get(controller)
                .ok_or_else(|| anyhow::anyhow!("controller routes missing: {controller}"))?;
            let (handler, route) = routes
                .iter()
                .find(|(h, _)| h.id.as_str() == handler_id)
                .ok_or_else(|| anyhow::anyhow!("handler missing: {handler_id}"))?;
            let ctrl_slug = crate::slugify::slugify(controller);
            let handler_slug = pages::api_flow::handler_slug(handler.id.as_str());
            let class_dev_slugs = ctx.dev_slugs_visible(Some(feature), rendered);
            let process_id = ctx.process_by_handler.get(handler.id.as_str());
            let flow_summary = process_id
                .and_then(|pid| input.flow_llm_summaries.as_ref()?.get(pid.as_str()))
                .or_else(|| input.flow_llm_summaries.as_ref()?.get(handler.id.as_str()));
            let content = pages::api_flow::render_api_flow_page(
                handler,
                route,
                *position,
                flow_summary,
                graph,
                &class_dev_slugs,
                &ctx.method_flow_desc,
            );
            let page_path = format!("{}/api/{}/{}", feature, ctrl_slug, handler_slug);
            let flow_title = pages::api_flow::handler_title(handler.id.as_str());
            Ok(flow_page(
                feature, &page_path, flow_title, "api-flow", content,
            ))
        }
        PageSubject::ScheduledFlow {
            feature,
            method_id,
            position,
        } => {
            let slug = pages::api_flow::handler_slug(method_id);
            let class_dev_slugs = ctx.dev_slugs_visible(None, rendered);
            let content = pages::api_flow::render_scheduled_flow_page(
                method_id,
                *position,
                graph,
                &class_dev_slugs,
                &ctx.all_method_desc,
            );
            let page_path = format!("{}/api/scheduled/{}", feature, slug);
            let flow_title = pages::api_flow::handler_title(method_id);
            Ok(flow_page(
                feature,
                &page_path,
                flow_title,
                "scheduled-flow",
                content,
            ))
        }
        PageSubject::ListenerFlow {
            feature,
            method_id,
            topics,
            position,
        } => {
            let slug = pages::api_flow::handler_slug(method_id);
            let class_dev_slugs = ctx.dev_slugs_visible(None, rendered);
            let content = pages::api_flow::render_listener_flow_page(
                method_id,
                topics.as_slice(),
                *position,
                graph,
                &class_dev_slugs,
                &ctx.all_method_desc,
            );
            let page_path = format!("{}/api/events/{}", feature, slug);
            let flow_title = pages::api_flow::handler_title(method_id);
            Ok(flow_page(
                feature,
                &page_path,
                flow_title,
                "listener-flow",
                content,
            ))
        }
        PageSubject::CommunityIndex => {
            let content = pages::community::render_community_index(
                &graph.community_nodes,
                &ctx.comm_slug_map,
                graph,
            );
            Ok(RenderedPage {
                rel_path: "pages/communities/index.md".into(),
                content,
                json: None,
                entry: Some(PageEntry {
                    slug: "communities/index".into(),
                    role: "communities".into(),
                    title: "Communities".into(),
                    kind: "index".into(),
                    path: "pages/communities/index.md".into(),
                    json_path: None,
                    community_id: None,
                }),
                nav: Vec::new(),
                agent_index: None,
            })
        }
        PageSubject::CommunityDetail { community_id } => {
            let comm = community_node(graph, community_id)?;
            let dir_name = ctx
                .comm_slug_map
                .get(community_id)
                .cloned()
                .unwrap_or_else(|| crate::slugify::slugify(community_id));
            let processes_here = processes_for_community(graph, community_id);
            let llm = input
                .llm_summaries
                .as_ref()
                .and_then(|m| m.get(community_id));
            let content =
                pages::community::render_community_detail(comm, graph, &processes_here, llm);
            Ok(RenderedPage {
                rel_path: format!("pages/communities/{dir_name}/index.md"),
                content,
                json: None,
                entry: Some(PageEntry {
                    slug: format!("communities/{dir_name}/index"),
                    role: "communities".into(),
                    title: comm.name.clone(),
                    kind: "index".into(),
                    path: format!("pages/communities/{dir_name}/index.md"),
                    json_path: None,
                    community_id: Some(community_id.clone()),
                }),
                nav: Vec::new(),
                agent_index: None,
            })
        }
        PageSubject::CommunityPo { community_id } => {
            let comm = community_node(graph, community_id)?;
            let dir_name = ctx
                .comm_slug_map
                .get(community_id)
                .cloned()
                .unwrap_or_else(|| crate::slugify::slugify(community_id));
            let full = input
                .llm_full
                .as_ref()
                .and_then(|m| m.get(community_id))
                .ok_or_else(|| anyhow::anyhow!("no llm_full for {community_id}"))?;
            let content = pages::community::render_community_po(comm, graph, full);
            Ok(RenderedPage {
                rel_path: format!("pages/communities/{dir_name}/po.md"),
                content,
                json: None,
                entry: Some(PageEntry {
                    slug: format!("communities/{dir_name}/po"),
                    role: "communities".into(),
                    title: format!("{} — Business Overview", comm.name),
                    kind: "po".into(),
                    path: format!("pages/communities/{dir_name}/po.md"),
                    json_path: None,
                    community_id: Some(community_id.clone()),
                }),
                nav: Vec::new(),
                agent_index: None,
            })
        }
        PageSubject::CommunityBa { community_id } => {
            let comm = community_node(graph, community_id)?;
            let dir_name = ctx
                .comm_slug_map
                .get(community_id)
                .cloned()
                .unwrap_or_else(|| crate::slugify::slugify(community_id));
            let full = input
                .llm_full
                .as_ref()
                .and_then(|m| m.get(community_id))
                .ok_or_else(|| anyhow::anyhow!("no llm_full for {community_id}"))?;
            let processes_here = processes_for_community(graph, community_id);
            let content = pages::community::render_community_ba(comm, graph, &processes_here, full);
            Ok(RenderedPage {
                rel_path: format!("pages/communities/{dir_name}/ba.md"),
                content,
                json: None,
                entry: Some(PageEntry {
                    slug: format!("communities/{dir_name}/ba"),
                    role: "communities".into(),
                    title: format!("{} — Business Analysis", comm.name),
                    kind: "ba".into(),
                    path: format!("pages/communities/{dir_name}/ba.md"),
                    json_path: None,
                    community_id: Some(community_id.clone()),
                }),
                nav: Vec::new(),
                agent_index: None,
            })
        }
    }
}

fn feature_page(feature: &str, kind: &str, title: String, content: String) -> RenderedPage {
    RenderedPage {
        rel_path: format!("pages/{}/{}.md", feature, kind),
        content,
        json: None,
        entry: Some(PageEntry {
            slug: format!("{}/{}", feature, kind),
            role: feature.to_string(),
            title: title.clone(),
            kind: kind.to_string(),
            path: format!("pages/{}/{}.md", feature, kind),
            json_path: None,
            community_id: None,
        }),
        nav: vec![(
            feature.to_string(),
            NavEntry {
                slug: format!("{}/{}", feature, kind),
                title,
                kind: kind.to_string(),
            },
        )],
        agent_index: None,
    }
}

fn flow_page(
    feature: &str,
    page_path: &str,
    title: String,
    kind: &str,
    content: String,
) -> RenderedPage {
    RenderedPage {
        rel_path: format!("pages/{}.md", page_path),
        content,
        json: None,
        entry: Some(PageEntry {
            slug: page_path.to_string(),
            role: feature.to_string(),
            title: title.clone(),
            kind: kind.to_string(),
            path: format!("pages/{}.md", page_path),
            json_path: None,
            community_id: None,
        }),
        nav: vec![(
            feature.to_string(),
            NavEntry {
                slug: page_path.to_string(),
                title,
                kind: kind.to_string(),
            },
        )],
        agent_index: None,
    }
}

fn community_node<'g>(graph: &'g WikiGraph, community_id: &str) -> Result<&'g Node> {
    graph
        .community_nodes
        .iter()
        .find(|c| c.id.as_str() == community_id)
        .ok_or_else(|| anyhow::anyhow!("community node missing: {community_id}"))
}

fn processes_for_community<'g>(graph: &'g WikiGraph, community_id: &str) -> Vec<&'g Node> {
    graph
        .process_nodes
        .iter()
        .filter(|p| {
            p.props
                .as_ref()
                .and_then(|props| props.get("communities"))
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().any(|x| x.as_str() == Some(community_id)))
                .unwrap_or(false)
        })
        .collect()
}
