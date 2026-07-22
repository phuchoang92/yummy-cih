//! Typed `architecture_overview` application service composed live over existing
//! `GraphStore` port methods plus labeled artifact reads (entrypoints sidecar,
//! wiki index, registry). Design record: `docs/plans/architecture-overview-tool.md`
//! (D1–D6). No new port methods, no precomputed artifact: the motivating bug was
//! a stale precomputed snapshot, so every graph-sourced section is computed at
//! call time and every non-graph section carries a one-word `source` label.
//!
//! Shaping contract (D5): per-section item caps scaled by one `limit` knob,
//! deterministic ordering everywhere (golden tests + prompt caching), a ~32KB
//! byte backstop that drops whole trailing sections in [`DROP_ORDER`], and
//! `next` hints in exact tool-call syntax (guarded by a router test in `app.rs`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use cih_core::RegistryEntry;
use cih_graph_store::{
    CommunityInfo, GraphOverview, GraphStore, GraphStoreError, GraphSummary, HotspotNode,
    KindCount, Result as StoreResult, RouteInfo,
};

use crate::domain::error::AppError;
use crate::domain::repository::{RepoSelector, ResolvedRepo};
use crate::ports::blocking_runtime::{blocking_timeout, run_blocking};
use crate::ports::repo_context_provider::RepoContextProvider;

/// Hard response-size backstop. Approximate by design: a small margin is
/// reserved so the drop warning itself cannot push the response back over.
const MAX_RESPONSE_BYTES: usize = 32 * 1024;
const BACKSTOP_MARGIN_BYTES: usize = 512;

/// `limit` is a plain max-items-per-list, clamped to this (D5 — not a multiplier).
const HARD_ITEM_CAP: usize = 100;

const DEFAULT_MODULES: usize = 15;
const DEFAULT_ROUTE_GROUPS: usize = 20;
const DEFAULT_HUBS: usize = 10;
const DEFAULT_SCHEDULED: usize = 10;
const DEFAULT_WIKI_PAGES: usize = 10;
const DEFAULT_HOTSPOTS: usize = 10;
const ANCHORS_PER_MODULE: usize = 3;
/// Candidate pool for anchor symbols + hubs (one `graph_overview` call serves both).
const OVERVIEW_NODE_POOL: usize = 256;
/// Ceiling on the route fetch when sizing from the live Route count.
const MAX_ROUTE_FETCH: usize = 20_000;

// Section names, defined once: the selection arrays below, `compose`'s
// wanted-section checks, and `OverviewResponse::drop_section` all reference
// these consts, and `section_wiring_is_consistent` +
// `every_drop_order_entry_actually_drops` pin the wiring when a section is added.
const SECTION_STATS: &str = "stats";
const SECTION_MODULES: &str = "modules";
const SECTION_ROUTE_GROUPS: &str = "route_groups";
const SECTION_ENTRYPOINTS: &str = "entrypoints";
const SECTION_WIKI_PAGES: &str = "wiki_pages";
const SECTION_HOTSPOTS: &str = "hotspots";

const VALID_SECTIONS: &[&str] = &[
    SECTION_STATS,
    SECTION_MODULES,
    SECTION_ROUTE_GROUPS,
    SECTION_ENTRYPOINTS,
    SECTION_WIKI_PAGES,
    SECTION_HOTSPOTS,
];

#[derive(Clone, Debug)]
pub(crate) struct ArchitectureOverviewCommand {
    repo: RepoSelector,
    sections: Vec<String>,
    limit: usize,
}

impl ArchitectureOverviewCommand {
    pub(crate) fn try_new(
        repo: String,
        sections: Vec<String>,
        limit: usize,
    ) -> Result<Self, AppError> {
        Ok(Self {
            repo: RepoSelector::from_wire(&repo),
            sections: validate_sections(&sections)?,
            limit: limit.min(HARD_ITEM_CAP),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct OverviewWikiPage {
    pub(crate) slug: String,
    pub(crate) title: String,
    pub(crate) kind: String,
}

#[derive(Clone, Debug)]
pub(crate) struct OverviewWikiListing {
    pub(crate) pages: Vec<OverviewWikiPage>,
    pub(crate) page_count: usize,
    pub(crate) source: &'static str,
    pub(crate) graph_version: Option<String>,
    pub(crate) generated_at: Option<String>,
}

#[async_trait]
pub(crate) trait OverviewWikiRepository: Send + Sync {
    async fn list_pages(
        &self,
        repo: &ResolvedRepo,
    ) -> Result<Option<OverviewWikiListing>, AppError>;
}

#[derive(Clone)]
pub(crate) struct ArchitectureOverviewService {
    repo_contexts: Arc<dyn RepoContextProvider>,
    wiki: Arc<dyn OverviewWikiRepository>,
}

impl ArchitectureOverviewService {
    pub(crate) fn new(
        repo_contexts: Arc<dyn RepoContextProvider>,
        wiki: Arc<dyn OverviewWikiRepository>,
    ) -> Self {
        Self {
            repo_contexts,
            wiki,
        }
    }

    pub(crate) async fn execute(
        &self,
        command: ArchitectureOverviewCommand,
    ) -> Result<OverviewResponse, AppError> {
        // Keep validation at the application boundary even though the public
        // constructor also validates. This prevents repository or graph work
        // if a future caller constructs commands inside this crate.
        let sections = validate_sections(&command.sections)?;
        let wants_wiki = sections.iter().any(|section| section == SECTION_WIKI_PAGES);
        let context = self.repo_contexts.resolve(command.repo).await?;
        let entry = &context.repo.registry_entry;
        let catalog = self.repo_contexts.catalog_snapshot();
        let repo_name = entry.name.clone();
        let sidecar_catalog = catalog.clone();
        let sidecars = run_blocking(
            blocking_timeout(),
            "architecture overview group sidecars",
            move || {
                let registry = sidecar_catalog.registry();
                (
                    registry.is_stale(&repo_name),
                    group_sections(&repo_name, registry, sidecar_catalog.groups()),
                )
            },
        );
        let wiki = async {
            if wants_wiki {
                self.wiki.list_pages(&context.repo).await
            } else {
                Ok(None)
            }
        };
        let (sidecars, wiki) = tokio::join!(
            async {
                sidecars.await.map_err(|error| AppError::Unavailable {
                    dependency: "architecture overview sidecars",
                    message: error.to_string(),
                    retryable: true,
                })
            },
            wiki
        );
        let (registry_stale, groups) = sidecars?;
        let (wiki, wiki_warning) = match wiki {
            Ok(wiki) => (wiki, None),
            Err(error) => (
                None,
                Some(format!(
                    "wiki page metadata unavailable ({error}); graph-backed overview sections remain current"
                )),
            ),
        };
        compose(ComposeCtx {
            store: context.store.as_ref(),
            entry,
            registry_stale,
            groups,
            wiki,
            wiki_warning,
            sections,
            limit: command.limit,
        })
        .await
    }
}
/// `hotspots` is opt-in (product decision 2026-07-19): complexity data during
/// orientation invites refactoring detours.
const DEFAULT_SECTIONS: &[&str] = &[
    SECTION_STATS,
    SECTION_MODULES,
    SECTION_ROUTE_GROUPS,
    SECTION_ENTRYPOINTS,
    SECTION_WIKI_PAGES,
];
/// Byte-backstop drop order: first entry dropped first. `stats`, `provenance`,
/// `warnings`, and `group` are never dropped.
const DROP_ORDER: &[&str] = &[
    SECTION_HOTSPOTS,
    SECTION_WIKI_PAGES,
    SECTION_ENTRYPOINTS,
    SECTION_ROUTE_GROUPS,
    SECTION_MODULES,
];
/// Every tool name a `next` hint may reference. `app.rs` has a guard test that
/// each of these is a registered route — a hint that drifts from a real tool
/// signature teaches clients hallucinated calls.
#[cfg(test)]
pub(crate) const HINT_TOOLS: &[&str] = &[
    "communities",
    "route_map",
    "get_wiki_page",
    "complexity_hotspots",
    "group_contracts",
    "architecture_overview",
];

mod remedy {
    use cih_core::RegistryEntry;

    pub(super) fn analyze(entry: &RegistryEntry) -> String {
        format!("cih-engine analyze {}", entry.path)
    }

    pub(super) fn discover(entry: &RegistryEntry) -> String {
        format!("cih-engine discover {}", entry.path)
    }

    pub(super) fn load(entry: &RegistryEntry) -> String {
        format!("cih-engine load {}", entry.path)
    }

    pub(super) fn wiki(entry: &RegistryEntry) -> String {
        format!("cih-engine wiki {}", entry.path)
    }
}

/// A section that is either served (with a one-word `source` label) or
/// explicitly unavailable with a reason + remedy. A requested section always
/// appears — `available: false` means "a pipeline step has not run" or "a query
/// failed", never "none found" (agents must not read absence as a codebase fact).
#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum Section<T: Serialize> {
    Available {
        available: bool,
        /// One of: graph | registry | artifact | wiki-live | wiki-bundle (D4).
        source: &'static str,
        #[serde(flatten)]
        body: T,
    },
    Unavailable {
        available: bool,
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        remedy: Option<String>,
    },
}

impl<T: Serialize> Section<T> {
    fn ok(source: &'static str, body: T) -> Self {
        Self::Available {
            available: true,
            source,
            body,
        }
    }

    fn off(reason: impl Into<String>, remedy: Option<String>) -> Self {
        Self::Unavailable {
            available: false,
            reason: reason.into(),
            remedy,
        }
    }

    /// Backend failure on a non-first query: per-section error, worded so an
    /// outage cannot masquerade as "discover never ran" (D5 error taxonomy).
    fn store_err(e: &GraphStoreError) -> Self {
        Self::off(
            format!("graph query failed: {e}"),
            Some("check the graph backend / server logs — this is a serving error, not a fact about the codebase".into()),
        )
    }
}

#[derive(Serialize)]
struct StatsBody {
    total_nodes: u64,
    total_edges: u64,
    kinds: Vec<KindCount>,
}

#[derive(Serialize)]
struct ModulesBody {
    /// Detected module clusters (graph communities) — not build modules.
    total: usize,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<String>,
    items: Vec<ModuleEntry>,
}

#[derive(Serialize)]
struct ModuleEntry {
    id: String,
    name: String,
    symbol_count: u64,
    cohesion: f64,
    /// Canonical NodeIds of top-degree members — ready to seed `context(name=...)`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    anchor_symbols: Vec<String>,
}

#[derive(Serialize)]
struct RouteGroupsBody {
    total_routes: usize,
    total_groups: usize,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<String>,
    items: Vec<RouteGroup>,
}

#[derive(Serialize)]
struct RouteGroup {
    prefix: String,
    count: usize,
    /// `Route:METHOD /path → Handler:NodeId` — both halves copy-pasteable
    /// (`trace_flow(entry_point=...)` / `context(name=...)`).
    samples: Vec<String>,
}

#[derive(Serialize)]
struct EntrypointsBody {
    /// Top-degree structural symbols (runtime entry points and hubs — not `main()`).
    hubs: Vec<HubEntry>,
    /// Scheduled / event-listener methods from the discover sidecar.
    scheduled: Section<ScheduledBody>,
}

#[derive(Serialize)]
struct HubEntry {
    id: String,
    kind: String,
    name: String,
    degree: u64,
}

#[derive(Serialize)]
struct ScheduledBody {
    total: usize,
    truncated: bool,
    items: Vec<EntrypointItem>,
}

#[derive(Serialize)]
struct EntrypointItem {
    id: String,
    kind: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    topics: Vec<String>,
}

#[derive(Serialize)]
struct WikiPagesBody {
    page_count: usize,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<String>,
    items: Vec<WikiPageRef>,
}

#[derive(Serialize)]
struct WikiPageRef {
    slug: String,
    title: String,
    kind: String,
}

#[derive(Serialize)]
struct HotspotsBody {
    /// True total is unknown (the query is limit-bounded); `truncated` says
    /// whether more exist beyond `items`.
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<String>,
    items: Vec<HotspotEntry>,
}

#[derive(Serialize)]
struct HotspotEntry {
    id: String,
    name: String,
    file: String,
    cyclomatic: u64,
    cognitive: u64,
}

/// Thin group block (D6): membership + contract freshness + one-line member
/// stats. Cross-repo structure stays in the dedicated tools (`next` points there).
#[derive(Serialize)]
struct GroupOut {
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) contracts_synced_at: Option<String>,
    pub(crate) contracts_stale: bool,
    pub(crate) members: Vec<GroupMemberOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next: Option<String>,
}

/// One-line member stats, labeled `registry` at the section level. `repo` is
/// the exact string accepted by every tool's `repo` argument.
#[derive(Serialize)]
struct GroupMemberOut {
    pub(crate) repo: String,
    pub(crate) nodes: usize,
    pub(crate) edges: usize,
    pub(crate) routes: usize,
    pub(crate) communities: usize,
    pub(crate) indexed_at: String,
}

#[derive(Serialize)]
struct GroupBody {
    groups: Vec<GroupOut>,
}

#[derive(Serialize)]
struct WikiClock {
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    graph_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generated_at: Option<String>,
}

/// One clock per source (D4). Deliberately NO call-time timestamps: identical
/// state must serialize to identical bytes (prompt caching, golden tests).
#[derive(Serialize)]
struct Provenance {
    graph_key: String,
    indexed_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_head: Option<String>,
    registry_stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifacts_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entrypoints_sidecar_mtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wiki: Option<WikiClock>,
}

/// The tool's full response (D5: a real struct from day one — it becomes the
/// output schema when structured content lands at `json_result`). Keep this
/// typed contract instead of a dynamic section registry: compile-time schema
/// stability is more valuable here than runtime extension indirection. Field
/// order is serialization order; `None` = not requested or dropped by the
/// backstop.
#[derive(Serialize)]
pub(crate) struct OverviewResponse {
    repo: String,
    provenance: Provenance,
    warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stats: Option<Section<StatsBody>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    modules: Option<Section<ModulesBody>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    route_groups: Option<Section<RouteGroupsBody>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entrypoints: Option<Section<EntrypointsBody>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wiki_pages: Option<Section<WikiPagesBody>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hotspots: Option<Section<HotspotsBody>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    group: Option<Section<GroupBody>>,
}

impl OverviewResponse {
    fn drop_section(&mut self, name: &str) -> bool {
        match name {
            SECTION_HOTSPOTS => self.hotspots.take().is_some(),
            SECTION_WIKI_PAGES => self.wiki_pages.take().is_some(),
            SECTION_ENTRYPOINTS => self.entrypoints.take().is_some(),
            SECTION_ROUTE_GROUPS => self.route_groups.take().is_some(),
            SECTION_MODULES => self.modules.take().is_some(),
            _ => false,
        }
    }
}

/// Composition depends on the server's established `GraphStore` port rather
/// than a private overview-only subtrait. A narrowed duplicate would add a
/// second abstraction over the same backend boundary and diverge from every
/// other tool; only the test fake pays for the wider interface.
struct ComposeCtx<'a> {
    store: &'a dyn GraphStore,
    entry: &'a RegistryEntry,
    registry_stale: bool,
    groups: Vec<GroupOut>,
    wiki: Option<OverviewWikiListing>,
    wiki_warning: Option<String>,
    sections: Vec<String>,
    limit: usize,
}

/// Effective per-list cap: `limit == 0` means the section default (D5).
fn cap(limit: usize, default_cap: usize) -> usize {
    if limit == 0 {
        default_cap
    } else {
        limit.clamp(1, HARD_ITEM_CAP)
    }
}

/// Hints must be copy-pasteable tool-call syntax; a value that could break the
/// quoting is worse than no hint at all.
fn hint_safe(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.' | '{' | '}' | '~' | '%')
        })
}

/// Grouping key for a route path: the leading segments up to and including the
/// first non-generic one (`api` and version segments like `v1` are generic).
fn route_prefix(path: &str) -> String {
    let mut out = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        out.push_str(seg);
        let version_like = seg.len() >= 2
            && (seg.starts_with('v') || seg.starts_with('V'))
            && seg[1..].chars().all(|c| c.is_ascii_digit());
        let generic = seg.eq_ignore_ascii_case("api") || version_like;
        if !generic || out.matches('/').count() >= 3 {
            break;
        }
    }
    if out.is_empty() {
        "/".to_string()
    } else {
        out
    }
}

fn build_route_groups(
    mut routes: Vec<RouteInfo>,
    total_routes: usize,
    cap: usize,
) -> RouteGroupsBody {
    routes.sort_by(|a, b| {
        (a.path.as_str(), a.http_method.as_str()).cmp(&(b.path.as_str(), b.http_method.as_str()))
    });
    let mut grouped: std::collections::BTreeMap<String, (usize, Vec<String>)> =
        std::collections::BTreeMap::new();
    for r in &routes {
        let entry = grouped.entry(route_prefix(&r.path)).or_default();
        entry.0 += 1;
        if entry.1.len() < 2 {
            entry.1.push(format!(
                "Route:{} {} → {}",
                r.http_method,
                r.path,
                r.handler_id.as_str()
            ));
        }
    }
    let total_groups = grouped.len();
    let mut items: Vec<RouteGroup> = grouped
        .into_iter()
        .map(|(prefix, (count, samples))| RouteGroup {
            prefix,
            count,
            samples,
        })
        .collect();
    items.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.prefix.cmp(&b.prefix)));
    items.truncate(cap);
    let truncated = total_groups > items.len();
    let next = (truncated && items.first().is_some_and(|g| hint_safe(&g.prefix)))
        .then(|| format!("route_map(prefix=\"{}\")", items[0].prefix));
    RouteGroupsBody {
        total_routes,
        total_groups,
        truncated,
        next,
        items,
    }
}

#[derive(Deserialize)]
struct SidecarRecord {
    method_id: String,
    kind: String,
    #[serde(default)]
    topics: Vec<String>,
}

/// Load `.cih/entrypoints.json`, disambiguating absence (risk 2 in the design
/// record: the sidecar is only written when non-empty, so a missing file can
/// mean either "discover never ran" or "nothing detected").
fn load_scheduled(
    entry: &RegistryEntry,
    item_cap: usize,
) -> (Section<ScheduledBody>, Option<String>) {
    let path = Path::new(&entry.path).join(".cih").join("entrypoints.json");
    let discover_ran = entry.community_artifacts_dir.is_some();
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) if !discover_ran => {
            return (
                Section::off(
                    "discover has not run for this index",
                    Some(remedy::discover(entry)),
                ),
                None,
            );
        }
        Err(_) => {
            return (
                Section::off(
                    "no scheduled/event entrypoints recorded — discover writes this sidecar only when it detects any",
                    None,
                ),
                None,
            );
        }
    };
    let mtime = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| cih_core::unix_secs_to_rfc3339(d.as_secs()));
    let mut records: Vec<SidecarRecord> = match serde_json::from_str(&raw) {
        Ok(records) => records,
        Err(e) => {
            return (
                Section::off(
                    format!("entrypoints sidecar unreadable: {e}"),
                    Some(format!("re-run: {}", remedy::discover(entry))),
                ),
                mtime,
            );
        }
    };
    records.sort_by(|a, b| {
        (a.kind.as_str(), a.method_id.as_str()).cmp(&(b.kind.as_str(), b.method_id.as_str()))
    });
    let total = records.len();
    let items: Vec<EntrypointItem> = records
        .into_iter()
        .take(item_cap)
        .map(|r| EntrypointItem {
            id: r.method_id,
            kind: r.kind,
            topics: r.topics,
        })
        .collect();
    let truncated = total > items.len();
    (
        Section::ok(
            "artifact",
            ScheduledBody {
                total,
                truncated,
                items,
            },
        ),
        mtime,
    )
}

fn build_wiki_pages(listing: &OverviewWikiListing, item_cap: usize) -> Section<WikiPagesBody> {
    fn kind_priority(kind: &str) -> u8 {
        match kind {
            "index" => 0,
            "routes" => 1,
            "api-flow" => 2,
            _ => 3,
        }
    }
    let mut pages: Vec<&OverviewWikiPage> = listing.pages.iter().collect();
    pages.sort_by(|a, b| {
        (kind_priority(&a.kind), a.slug.as_str()).cmp(&(kind_priority(&b.kind), b.slug.as_str()))
    });
    let items: Vec<WikiPageRef> = pages
        .into_iter()
        .take(item_cap)
        .map(|p| WikiPageRef {
            slug: p.slug.clone(),
            title: p.title.clone(),
            kind: p.kind.clone(),
        })
        .collect();
    let truncated = listing.page_count > items.len();
    let next = (truncated && items.first().is_some_and(|p| hint_safe(&p.slug)))
        .then(|| format!("get_wiki_page(slug=\"{}\")", items[0].slug));
    Section::ok(
        listing.source,
        WikiPagesBody {
            page_count: listing.page_count,
            truncated,
            next,
            items,
        },
    )
}

/// Assemble the group block from the registries — mirrors how `status` builds
/// its group view (`groups_containing`, not just the server's `--group`), so a
/// repo's group facts appear even when the server isn't group-fronted (D6).
fn group_sections(
    repo_name: &str,
    reg: &cih_core::Registry,
    groups: &cih_core::GroupRegistry,
) -> Vec<GroupOut> {
    groups
        .groups_containing(repo_name)
        .map(|group| {
            let state =
                cih_core::group_dir(&group.name).and_then(|dir| cih_core::SyncState::load(&dir));
            let contracts_exist =
                cih_core::contracts_path(&group.name).is_some_and(|path| path.exists());
            let contracts_stale =
                cih_core::group_contracts_stale(group, reg, state.as_ref(), contracts_exist);
            let members = group
                .repos
                .iter()
                .take(20)
                .map(|member| match reg.find(member) {
                    Some(e) => GroupMemberOut {
                        repo: member.clone(),
                        nodes: e.stats.nodes,
                        edges: e.stats.edges,
                        routes: e.stats.routes,
                        communities: e.stats.communities,
                        indexed_at: e.indexed_at.clone(),
                    },
                    None => GroupMemberOut {
                        repo: member.clone(),
                        nodes: 0,
                        edges: 0,
                        routes: 0,
                        communities: 0,
                        indexed_at: String::new(),
                    },
                })
                .collect();
            GroupOut {
                name: group.name.clone(),
                contracts_synced_at: state.map(|s| s.synced_at),
                contracts_stale,
                members,
                next: hint_safe(&group.name)
                    .then(|| format!("group_contracts(group=\"{}\")", group.name)),
            }
        })
        .collect()
}

fn validate_sections(sections: &[String]) -> Result<Vec<String>, AppError> {
    if sections.is_empty() {
        return Ok(DEFAULT_SECTIONS.iter().map(|s| s.to_string()).collect());
    }
    for section in sections {
        if !VALID_SECTIONS.contains(&section.as_str()) {
            return Err(AppError::InvalidInput {
                field: "sections",
                message: format!(
                    "unknown section '{}'; valid sections: {}",
                    section,
                    VALID_SECTIONS.join(", ")
                ),
            });
        }
    }
    Ok(sections.to_vec())
}

fn build_warnings(
    ctx: &ComposeCtx<'_>,
    summary: &GraphSummary,
    artifacts_version: Option<&str>,
) -> Vec<String> {
    let entry = ctx.entry;
    let mut warnings = Vec::new();
    if ctx.registry_stale {
        warnings.push(format!(
            "repo has commits newer than the index (git HEAD changed since {}) — re-run: {}",
            entry.indexed_at,
            remedy::analyze(entry)
        ));
    }
    if entry.stats.nodes == 0 {
        warnings.push(
            "registry stats for this repo are zero (discover has not run or stats were never recorded) — counts in this response come from the live graph"
                .into(),
        );
    } else {
        let live_nodes = summary.total_nodes as f64;
        let registry_nodes = entry.stats.nodes as f64;
        if (live_nodes - registry_nodes).abs() / live_nodes.max(registry_nodes).max(1.0) > 0.10 {
            warnings.push(format!(
                "graph store size ({} nodes) diverges from registry stats ({} nodes) — the loaded graph may not match the latest artifacts; reload: {}",
                summary.total_nodes,
                entry.stats.nodes,
                remedy::load(entry)
            ));
        }
    }

    if let Some(wiki) = &ctx.wiki {
        if wiki.source == "wiki-bundle" {
            if let (Some(graph_version), Some(artifacts_version)) =
                (wiki.graph_version.as_deref(), artifacts_version)
            {
                if !graph_version.is_empty() && graph_version != artifacts_version {
                    warnings.push(format!(
                        "wiki bundle describes an older index (graph_version {graph_version} ≠ current {artifacts_version}) — prefer graph-sourced data; regenerate: {}",
                        remedy::wiki(entry)
                    ));
                }
            }
        }
    }
    if let Some(warning) = &ctx.wiki_warning {
        warnings.push(warning.clone());
    }
    warnings
}

async fn build_modules(
    store: &dyn GraphStore,
    entry: &RegistryEntry,
    fetched: StoreResult<Vec<CommunityInfo>>,
    pool: Option<&StoreResult<GraphOverview>>,
    item_cap: usize,
    warnings: &mut Vec<String>,
) -> Section<ModulesBody> {
    match fetched {
        Err(e) => Section::store_err(&e),
        Ok(mut communities) if !communities.is_empty() => {
            communities.sort_by(|a, b| {
                b.symbol_count
                    .cmp(&a.symbol_count)
                    .then_with(|| a.id.cmp(&b.id))
            });
            let total = communities.len();
            communities.truncate(item_cap);

            // Anchor attribution is the only dependent query in this section.
            let mut anchors: HashMap<String, Vec<(u64, String)>> = HashMap::new();
            if let Some(pool) = pool.and_then(|result| result.as_ref().ok()) {
                let degrees: HashMap<&str, u64> = pool
                    .nodes
                    .iter()
                    .map(|node| (node.node.id.as_str(), node.degree))
                    .collect();
                let ids: Vec<cih_core::NodeId> =
                    pool.nodes.iter().map(|node| node.node.id.clone()).collect();
                match store.symbol_communities(&ids).await {
                    Ok(pairs) => {
                        for (id, community) in pairs {
                            let degree = degrees.get(id.as_str()).copied().unwrap_or(0);
                            anchors
                                .entry(community.id)
                                .or_default()
                                .push((degree, id.as_str().to_string()));
                        }
                    }
                    Err(e) => warnings.push(format!(
                        "anchor-symbol attribution unavailable (symbol_communities failed: {e}) — module rows carry no anchors"
                    )),
                }
            }

            let items = communities
                .into_iter()
                .map(|community| {
                    let anchor_symbols = anchors
                        .remove(&community.id)
                        .map(|mut list| {
                            list.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                            list.into_iter()
                                .take(ANCHORS_PER_MODULE)
                                .map(|(_, id)| id)
                                .collect()
                        })
                        .unwrap_or_default();
                    ModuleEntry {
                        id: community.id,
                        name: community.name,
                        symbol_count: community.symbol_count,
                        cohesion: community.cohesion,
                        anchor_symbols,
                    }
                })
                .collect::<Vec<_>>();
            let truncated = total > items.len();
            Section::ok(
                "graph",
                ModulesBody {
                    total,
                    truncated,
                    next: truncated.then(|| "communities()".to_string()),
                    items,
                },
            )
        }
        Ok(_) if entry.community_artifacts_dir.is_none() => Section::off(
            "discover has not run for this index (no module clusters in the graph)",
            Some(remedy::discover(entry)),
        ),
        Ok(_) => Section::off(
            "graph contains no Community nodes although discover artifacts exist — the loaded graph may predate discover",
            Some(remedy::load(entry)),
        ),
    }
}

async fn build_entrypoints(
    entry: &RegistryEntry,
    pool: Option<&StoreResult<GraphOverview>>,
    hub_cap: usize,
    scheduled_cap: usize,
) -> (Section<EntrypointsBody>, Option<String>) {
    if let Some(Err(e)) = pool {
        return (Section::store_err(e), None);
    }

    let mut hubs: Vec<HubEntry> = pool
        .and_then(|result| result.as_ref().ok())
        .map(|pool| {
            pool.nodes
                .iter()
                .map(|node| HubEntry {
                    id: node.node.id.as_str().to_string(),
                    kind: node.node.kind.label().to_string(),
                    name: node.node.name.clone(),
                    degree: node.degree,
                })
                .collect()
        })
        .unwrap_or_default();
    hubs.sort_by(|a, b| b.degree.cmp(&a.degree).then_with(|| a.id.cmp(&b.id)));
    hubs.truncate(hub_cap);
    let entry_for_sidecar = entry.clone();
    let (scheduled, sidecar_mtime) = match run_blocking(
        blocking_timeout(),
        "architecture overview entrypoints sidecar",
        move || load_scheduled(&entry_for_sidecar, scheduled_cap),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => (
            Section::off(
                format!("entrypoints sidecar unavailable: {error}"),
                Some(remedy::discover(entry)),
            ),
            None,
        ),
    };
    (
        Section::ok("graph", EntrypointsBody { hubs, scheduled }),
        sidecar_mtime,
    )
}

fn build_hotspots(
    fetched: StoreResult<Vec<HotspotNode>>,
    item_cap: usize,
) -> Section<HotspotsBody> {
    match fetched {
        Err(e) => Section::store_err(&e),
        Ok(mut nodes) => {
            nodes.sort_by(|a, b| {
                (b.cyclomatic + b.cognitive)
                    .cmp(&(a.cyclomatic + a.cognitive))
                    .then_with(|| a.id.as_str().cmp(b.id.as_str()))
            });
            let truncated = nodes.len() > item_cap;
            nodes.truncate(item_cap);
            Section::ok(
                "graph",
                HotspotsBody {
                    truncated,
                    next: truncated.then(|| "complexity_hotspots()".to_string()),
                    items: nodes
                        .into_iter()
                        .map(|node| HotspotEntry {
                            id: node.id.as_str().to_string(),
                            name: node.name,
                            file: node.file,
                            cyclomatic: node.cyclomatic,
                            cognitive: node.cognitive,
                        })
                        .collect(),
                },
            )
        }
    }
}

fn build_route_section(
    fetched: Option<StoreResult<Vec<RouteInfo>>>,
    route_total: usize,
    item_cap: usize,
) -> Section<RouteGroupsBody> {
    let Some(fetched) = fetched else {
        debug_assert_eq!(route_total, 0, "routes must be fetched when any exist");
        return Section::ok(
            "graph",
            RouteGroupsBody {
                total_routes: 0,
                total_groups: 0,
                truncated: false,
                next: None,
                items: vec![],
            },
        );
    };

    match fetched {
        Err(e) => Section::store_err(&e),
        Ok(routes) => Section::ok("graph", build_route_groups(routes, route_total, item_cap)),
    }
}

async fn compose(ctx: ComposeCtx<'_>) -> Result<OverviewResponse, AppError> {
    let selected = validate_sections(&ctx.sections)?;
    let want = |name: &str| selected.iter().any(|s| s == name);
    let entry = ctx.entry;

    // First store call: a backend error here means the store is down — hard
    // error (D5 taxonomy). Every later store error degrades per-section.
    let mut summary = ctx
        .store
        .graph_summary()
        .await
        .map_err(|error| AppError::Unavailable {
            dependency: "graph store",
            message: format!("architecture overview summary: {error}"),
            retryable: true,
        })?;
    summary
        .kinds
        .sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.kind.cmp(&b.kind)));
    let route_total = summary
        .kinds
        .iter()
        .find(|k| k.kind == "Route")
        .map(|k| k.count as usize)
        .unwrap_or(0);

    let artifacts_version = Path::new(&entry.artifacts_dir)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned());
    let mut warnings = build_warnings(&ctx, &summary, artifacts_version.as_deref());

    let want_stats = want(SECTION_STATS);
    let want_modules = want(SECTION_MODULES);
    let want_route_groups = want(SECTION_ROUTE_GROUPS);
    let want_entrypoints = want(SECTION_ENTRYPOINTS);
    let want_wiki_pages = want(SECTION_WIKI_PAGES);
    let want_hotspots = want(SECTION_HOTSPOTS);
    let need_pool = want_modules || want_entrypoints;
    let symbol_kinds: Vec<String> = ["Class", "Interface", "Function", "Method"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    // These reads are independent. The backend's query semaphore remains the
    // concurrency bound; symbol-community attribution runs later because it
    // depends on the overview pool.
    let (pool, fetched_communities, fetched_routes, fetched_hotspots) = tokio::join!(
        async {
            if need_pool {
                Some(
                    ctx.store
                        .graph_overview(OVERVIEW_NODE_POOL, 1, Some(&symbol_kinds))
                        .await,
                )
            } else {
                None
            }
        },
        async {
            if want_modules {
                Some(ctx.store.communities().await)
            } else {
                None
            }
        },
        async {
            if want_route_groups && route_total > 0 {
                // The 1..1000 clamp is tool-level (`route_map` tool); the port
                // takes a bare usize, so this enumerates the live Route count.
                let fetch = route_total.clamp(1, MAX_ROUTE_FETCH);
                Some(ctx.store.route_map(None, fetch).await)
            } else {
                None
            }
        },
        async {
            if want_hotspots {
                let item_cap = cap(ctx.limit, DEFAULT_HOTSPOTS);
                Some(
                    ctx.store
                        .complexity_hotspots(None, None, None, item_cap + 1)
                        .await,
                )
            } else {
                None
            }
        }
    );

    let stats = want_stats.then(|| {
        Section::ok(
            "graph",
            StatsBody {
                total_nodes: summary.total_nodes,
                total_edges: summary.total_edges,
                kinds: summary.kinds.clone(),
            },
        )
    });

    let modules = if want_modules {
        let fetched = fetched_communities
            .expect("communities are fetched whenever the modules section is selected");
        Some(
            build_modules(
                ctx.store,
                entry,
                fetched,
                pool.as_ref(),
                cap(ctx.limit, DEFAULT_MODULES),
                &mut warnings,
            )
            .await,
        )
    } else {
        None
    };

    let route_groups = if want_route_groups {
        Some(build_route_section(
            fetched_routes,
            route_total,
            cap(ctx.limit, DEFAULT_ROUTE_GROUPS),
        ))
    } else {
        None
    };

    let (entrypoints, sidecar_mtime) = if want_entrypoints {
        let (section, mtime) = build_entrypoints(
            entry,
            pool.as_ref(),
            cap(ctx.limit, DEFAULT_HUBS),
            cap(ctx.limit, DEFAULT_SCHEDULED),
        )
        .await;
        (Some(section), mtime)
    } else {
        (None, None)
    };

    let wiki_pages = if want_wiki_pages {
        Some(match &ctx.wiki {
            Some(listing) => build_wiki_pages(listing, cap(ctx.limit, DEFAULT_WIKI_PAGES)),
            None if ctx.wiki_warning.is_some() => Section::off(
                "wiki page metadata is temporarily unavailable",
                Some(
                    "retry the overview; graph-backed sections in this response remain valid"
                        .into(),
                ),
            ),
            None => Section::off("no generated wiki for this repo", Some(remedy::wiki(entry))),
        })
    } else {
        None
    };

    let hotspots = if want_hotspots {
        let fetched = fetched_hotspots
            .expect("hotspots are fetched whenever the hotspots section is selected");
        Some(build_hotspots(fetched, cap(ctx.limit, DEFAULT_HOTSPOTS)))
    } else {
        None
    };

    let group =
        (!ctx.groups.is_empty()).then(|| Section::ok("registry", GroupBody { groups: ctx.groups }));

    let wiki_clock = ctx.wiki.as_ref().map(|w| WikiClock {
        source: w.source,
        graph_version: w.graph_version.clone(),
        generated_at: w.generated_at.clone(),
    });

    let mut response = OverviewResponse {
        repo: entry.name.clone(),
        provenance: Provenance {
            graph_key: entry.graph_key.clone(),
            indexed_at: entry.indexed_at.clone(),
            git_head: entry.last_git_head.clone(),
            registry_stale: ctx.registry_stale,
            artifacts_version,
            entrypoints_sidecar_mtime: sidecar_mtime,
            wiki: wiki_clock,
        },
        warnings,
        stats,
        modules,
        route_groups,
        entrypoints,
        wiki_pages,
        hotspots,
        group,
    };

    // Byte backstop: drop whole trailing sections (never mid-list) in declared
    // order until the response fits, then say exactly what was dropped and how
    // to re-fetch it.
    let mut dropped: Vec<&'static str> = Vec::new();
    loop {
        let size = serde_json::to_vec(&response)
            .map(|v| v.len())
            .map_err(|error| AppError::Unavailable {
                dependency: "response serialization",
                message: error.to_string(),
                retryable: false,
            })?;
        if size + BACKSTOP_MARGIN_BYTES <= MAX_RESPONSE_BYTES {
            break;
        }
        let Some(name) = DROP_ORDER.iter().find(|name| response.drop_section(name)) else {
            break;
        };
        dropped.push(name);
    }
    if !dropped.is_empty() {
        response.warnings.push(format!(
            "response byte cap (~32KB) reached — dropped sections: {}; re-fetch each with architecture_overview(sections=[\"{}\"], limit=5)",
            dropped.join(", "),
            dropped[0]
        ));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::repository::RepoCatalogSnapshot;
    use crate::ports::repo_context_provider::RepoContext;
    use async_trait::async_trait;
    use cih_core::{Node, NodeId, NodeKind, RegistryEntry, RegistryStats};
    use cih_graph_store::{
        CommunityEdge, CommunityInfo, Direction, GraphOverview, GraphOverviewNode, GraphSummary,
        HotspotNode, Impact, Path as GraphPath, Result as StoreResult, SimilarMethod, Subgraph,
        SymbolContext,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Canned-data store: only the methods `compose` touches return data; the
    /// rest are unreachable from the overview and answer `Unimplemented`.
    #[derive(Default)]
    struct FakeStore {
        kinds: Vec<KindCount>,
        total_nodes: u64,
        total_edges: u64,
        communities: Vec<CommunityInfo>,
        routes: Vec<RouteInfo>,
        pool: Vec<GraphOverviewNode>,
        memberships: Vec<(NodeId, CommunityInfo)>,
        hotspots: Vec<HotspotNode>,
        fail_summary: bool,
        fail_communities: bool,
    }

    struct FixedRepoContexts {
        context: Arc<RepoContext>,
        catalog: RepoCatalogSnapshot,
    }

    #[async_trait]
    impl RepoContextProvider for FixedRepoContexts {
        fn catalog_snapshot(&self) -> RepoCatalogSnapshot {
            self.catalog.clone()
        }

        fn resolve_repo(&self, _selector: RepoSelector) -> Result<ResolvedRepo, AppError> {
            Ok(self.context.repo.clone())
        }

        async fn resolve(&self, _selector: RepoSelector) -> Result<Arc<RepoContext>, AppError> {
            Ok(self.context.clone())
        }
    }

    struct FixedWiki {
        listing: Option<OverviewWikiListing>,
    }

    #[async_trait]
    impl OverviewWikiRepository for FixedWiki {
        async fn list_pages(
            &self,
            _repo: &ResolvedRepo,
        ) -> Result<Option<OverviewWikiListing>, AppError> {
            Ok(self.listing.clone())
        }
    }

    struct UnavailableWiki {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl OverviewWikiRepository for UnavailableWiki {
        async fn list_pages(
            &self,
            _repo: &ResolvedRepo,
        ) -> Result<Option<OverviewWikiListing>, AppError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(AppError::Unavailable {
                dependency: "test wiki",
                message: "offline".into(),
                retryable: true,
            })
        }
    }

    fn unimpl<T>() -> StoreResult<T> {
        Err(GraphStoreError::Unimplemented("fake store"))
    }

    #[async_trait]
    impl GraphStore for FakeStore {
        async fn ensure_schema(&self) -> StoreResult<()> {
            Ok(())
        }
        async fn bulk_load(
            &self,
            _artifacts: &cih_core::GraphArtifacts,
        ) -> StoreResult<cih_graph_store::LoadStats> {
            unimpl()
        }
        async fn upsert_incremental(&self, _delta: &cih_core::GraphDelta) -> StoreResult<()> {
            unimpl()
        }
        async fn publish_to(&self, _dest_key: &str) -> StoreResult<()> {
            unimpl()
        }
        async fn drop_graph(&self) -> StoreResult<()> {
            unimpl()
        }
        async fn get_node(&self, _id: &NodeId) -> StoreResult<Option<Node>> {
            unimpl()
        }
        async fn neighbors(
            &self,
            _id: &NodeId,
            _dir: Direction,
            _kinds: &[cih_core::EdgeKind],
        ) -> StoreResult<Vec<cih_core::Edge>> {
            unimpl()
        }
        async fn impact(
            &self,
            _id: &NodeId,
            _dir: Direction,
            _max_depth: u32,
        ) -> StoreResult<Impact> {
            unimpl()
        }
        async fn call_chain(
            &self,
            _from: &NodeId,
            _to: &NodeId,
            _max_depth: u32,
        ) -> StoreResult<Vec<GraphPath>> {
            unimpl()
        }
        async fn subgraph(&self, _seeds: &[NodeId], _radius: u32) -> StoreResult<Subgraph> {
            unimpl()
        }
        async fn graph_summary(&self) -> StoreResult<GraphSummary> {
            if self.fail_summary {
                return Err(GraphStoreError::Backend("summary down".into()));
            }
            Ok(GraphSummary {
                kinds: self.kinds.clone(),
                total_nodes: self.total_nodes,
                total_edges: self.total_edges,
            })
        }
        async fn graph_overview(
            &self,
            _max_nodes: usize,
            _max_edges: usize,
            _kinds: Option<&[String]>,
        ) -> StoreResult<GraphOverview> {
            Ok(GraphOverview {
                nodes: self
                    .pool
                    .iter()
                    .map(|n| GraphOverviewNode {
                        node: n.node.clone(),
                        degree: n.degree,
                    })
                    .collect(),
                edges: vec![],
                total_nodes: self.total_nodes,
                total_edges: self.total_edges,
                truncated: false,
            })
        }
        async fn context(&self, _id: &NodeId) -> StoreResult<SymbolContext> {
            unimpl()
        }
        async fn communities(&self) -> StoreResult<Vec<CommunityInfo>> {
            if self.fail_communities {
                return Err(GraphStoreError::Backend("communities down".into()));
            }
            Ok(self.communities.clone())
        }
        async fn route_map(
            &self,
            _prefix: Option<&str>,
            limit: usize,
        ) -> StoreResult<Vec<RouteInfo>> {
            Ok(self.routes.iter().take(limit).cloned().collect())
        }
        async fn candidates_by_name(&self, _name: &str, _limit: usize) -> StoreResult<Vec<Node>> {
            unimpl()
        }
        async fn nodes_in_files(&self, _files: &[String]) -> StoreResult<Vec<Node>> {
            unimpl()
        }
        async fn processes_for_symbols(&self, _ids: &[NodeId]) -> StoreResult<Vec<String>> {
            unimpl()
        }
        async fn flow_downstream(
            &self,
            _entry: &NodeId,
            _max_depth: u32,
        ) -> StoreResult<Vec<cih_graph_store::FlowHop>> {
            unimpl()
        }
        async fn complexity_hotspots(
            &self,
            _min_cyclomatic: Option<u16>,
            _min_cognitive: Option<u16>,
            _min_transitive_loop: Option<u8>,
            limit: usize,
        ) -> StoreResult<Vec<HotspotNode>> {
            Ok(self.hotspots.iter().take(limit).cloned().collect())
        }
        async fn similar_methods(
            &self,
            _id: &NodeId,
            _min_jaccard: f32,
            _limit: usize,
        ) -> StoreResult<Vec<SimilarMethod>> {
            unimpl()
        }
        async fn symbol_communities(
            &self,
            ids: &[NodeId],
        ) -> StoreResult<Vec<(NodeId, CommunityInfo)>> {
            Ok(self
                .memberships
                .iter()
                .filter(|(id, _)| ids.contains(id))
                .cloned()
                .collect())
        }
        async fn test_coverage(&self, _id: &NodeId) -> StoreResult<Vec<Node>> {
            unimpl()
        }
        async fn tests_for_files(&self, _files: &[String]) -> StoreResult<Vec<Node>> {
            unimpl()
        }
        async fn untested_symbols(&self, _prefix: &str, _limit: usize) -> StoreResult<Vec<Node>> {
            unimpl()
        }
        async fn community_graph(&self) -> StoreResult<Vec<CommunityEdge>> {
            unimpl()
        }
    }

    fn node(id: &str, kind: NodeKind, name: &str) -> Node {
        Node {
            id: NodeId::new(id.to_string()),
            kind,
            name: name.to_string(),
            qualified_name: None,
            file: String::new(),
            range: Default::default(),
            props: None,
        }
    }

    fn community(id: &str, name: &str, symbol_count: u64) -> CommunityInfo {
        CommunityInfo {
            id: id.to_string(),
            name: name.to_string(),
            symbol_count,
            cohesion: 0.5,
        }
    }

    fn route(method: &str, path: &str, handler: &str) -> RouteInfo {
        RouteInfo {
            path: path.to_string(),
            http_method: method.to_string(),
            decorator: String::new(),
            handler_id: NodeId::new(handler.to_string()),
            handler_name: handler.to_string(),
            handler_qualified: handler.to_string(),
        }
    }

    fn entry(path: &str, community_dir: Option<&str>, stats_nodes: usize) -> RegistryEntry {
        RegistryEntry {
            name: "demo".into(),
            path: path.to_string(),
            graph_key: "demo".into(),
            artifacts_dir: format!("{path}/.cih/artifacts/deadbeef"),
            community_artifacts_dir: community_dir.map(str::to_string),
            indexed_at: "2026-07-19T00:00:00Z".into(),
            last_git_head: None,
            stats: RegistryStats {
                nodes: stats_nodes,
                ..Default::default()
            },
        }
    }

    fn populated_store() -> FakeStore {
        FakeStore {
            kinds: vec![
                KindCount {
                    kind: "Method".into(),
                    count: 80,
                },
                KindCount {
                    kind: "Route".into(),
                    count: 3,
                },
            ],
            total_nodes: 100,
            total_edges: 200,
            communities: vec![community("c1", "loan", 60), community("c2", "savings", 40)],
            routes: vec![
                route("GET", "/api/v1/loans/{id}", "Method:acme.LoanApi#get"),
                route("POST", "/api/v1/loans", "Method:acme.LoanApi#create"),
                route("GET", "/health", "Method:acme.Health#ping"),
            ],
            pool: vec![
                GraphOverviewNode {
                    node: node("Class:acme.LoanApi", NodeKind::Class, "LoanApi"),
                    degree: 9,
                },
                GraphOverviewNode {
                    node: node("Class:acme.Ledger", NodeKind::Class, "Ledger"),
                    degree: 4,
                },
            ],
            memberships: vec![
                (
                    NodeId::new("Class:acme.LoanApi".to_string()),
                    community("c1", "loan", 60),
                ),
                (
                    NodeId::new("Class:acme.Ledger".to_string()),
                    community("c2", "savings", 40),
                ),
            ],
            hotspots: vec![HotspotNode {
                id: NodeId::new("Method:acme.LoanApi#big".to_string()),
                name: "big".into(),
                file: "LoanApi.java".into(),
                cyclomatic: 30,
                cognitive: 40,
                transitive_loop_depth: 2,
            }],
            ..Default::default()
        }
    }

    fn ctx_with<'a>(
        store: &'a FakeStore,
        entry: &'a RegistryEntry,
        sections: Vec<String>,
        limit: usize,
    ) -> ComposeCtx<'a> {
        ComposeCtx {
            store,
            entry,
            registry_stale: false,
            groups: vec![],
            wiki: None,
            wiki_warning: None,
            sections,
            limit,
        }
    }

    async fn compose_json(ctx: ComposeCtx<'_>) -> serde_json::Value {
        let resp = compose(ctx).await.expect("compose should succeed");
        serde_json::to_value(&resp).expect("serializable")
    }

    #[tokio::test]
    async fn service_resolves_repo_and_wiki_through_typed_ports() {
        let registry_entry = entry("/nonexistent/demo", Some("/x"), 100);
        let context = Arc::new(RepoContext {
            repo: ResolvedRepo::from_entry(registry_entry.clone()),
            store: Arc::new(populated_store()),
            search: Arc::new(crate::infrastructure::search_provider::SearchState::new(
                None, None,
            )),
        });
        let catalog = RepoCatalogSnapshot::for_test(
            "demo".into(),
            cih_core::Registry {
                entries: vec![registry_entry],
            },
            cih_core::GroupRegistry::default(),
        );
        let service = ArchitectureOverviewService::new(
            Arc::new(FixedRepoContexts { context, catalog }),
            Arc::new(FixedWiki {
                listing: Some(OverviewWikiListing {
                    pages: vec![OverviewWikiPage {
                        slug: "index".into(),
                        title: "Overview".into(),
                        kind: "index".into(),
                    }],
                    page_count: 1,
                    source: "test-wiki",
                    graph_version: Some("v1".into()),
                    generated_at: None,
                }),
            }),
        );
        let command = ArchitectureOverviewCommand::try_new(
            String::new(),
            vec!["stats".into(), "wiki_pages".into()],
            5,
        )
        .unwrap();

        let output = service.execute(command).await.unwrap();
        let json = serde_json::to_value(output).unwrap();

        assert_eq!(json["repo"], "demo");
        assert_eq!(json["stats"]["available"], true);
        assert_eq!(json["wiki_pages"]["source"], "test-wiki");
        assert_eq!(json["wiki_pages"]["items"][0]["slug"], "index");
    }

    #[tokio::test]
    async fn service_skips_unrequested_wiki_and_degrades_requested_failure() {
        let registry_entry = entry("/nonexistent/demo", Some("/x"), 100);
        let context = Arc::new(RepoContext {
            repo: ResolvedRepo::from_entry(registry_entry.clone()),
            store: Arc::new(populated_store()),
            search: Arc::new(crate::infrastructure::search_provider::SearchState::new(
                None, None,
            )),
        });
        let catalog = RepoCatalogSnapshot::for_test(
            "demo".into(),
            cih_core::Registry {
                entries: vec![registry_entry],
            },
            cih_core::GroupRegistry::default(),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let service = ArchitectureOverviewService::new(
            Arc::new(FixedRepoContexts { context, catalog }),
            Arc::new(UnavailableWiki {
                calls: calls.clone(),
            }),
        );

        let stats = service
            .execute(
                ArchitectureOverviewCommand::try_new(String::new(), vec![SECTION_STATS.into()], 5)
                    .unwrap(),
            )
            .await
            .expect("unrequested wiki must not affect overview");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(stats.wiki_pages.is_none());

        let with_wiki = service
            .execute(
                ArchitectureOverviewCommand::try_new(
                    String::new(),
                    vec![SECTION_STATS.into(), SECTION_WIKI_PAGES.into()],
                    5,
                )
                .unwrap(),
            )
            .await
            .expect("wiki failure must degrade to a partial overview");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(with_wiki
            .warnings
            .iter()
            .any(|warning| warning.contains("wiki page metadata unavailable")));
        let wiki = serde_json::to_value(with_wiki.wiki_pages).unwrap();
        assert_eq!(wiki["available"], false);
        assert!(wiki["reason"]
            .as_str()
            .unwrap()
            .contains("temporarily unavailable"));
    }

    #[test]
    fn section_wiring_is_consistent() {
        for &section in DEFAULT_SECTIONS {
            assert!(
                VALID_SECTIONS.contains(&section),
                "default section '{section}' must be valid"
            );
        }
        for &section in DROP_ORDER {
            assert!(
                VALID_SECTIONS.contains(&section),
                "droppable section '{section}' must be valid"
            );
        }
        assert!(VALID_SECTIONS.contains(&SECTION_STATS));
        assert!(
            !DROP_ORDER.contains(&SECTION_STATS),
            "stats must never be droppable"
        );
    }

    #[tokio::test]
    async fn every_drop_order_entry_actually_drops() {
        let store = populated_store();
        let entry = entry("/nonexistent/demo", Some("/x"), 100);
        let sections = VALID_SECTIONS
            .iter()
            .map(|section| (*section).to_string())
            .collect();
        let mut response = compose(ctx_with(&store, &entry, sections, 0))
            .await
            .expect("all sections should compose");

        for &section in DROP_ORDER {
            assert!(
                response.drop_section(section),
                "drop order entry '{section}' must map to a populated response field"
            );
        }
        assert!(!response.drop_section(SECTION_STATS));
        assert!(
            response.stats.is_some(),
            "stats must survive manual dropping"
        );
    }

    #[tokio::test]
    async fn default_sections_carry_labels_and_exclude_hotspots() {
        let store = populated_store();
        let entry = entry(
            "/nonexistent/demo",
            Some("/nonexistent/demo/.cih/comm"),
            100,
        );
        let v = compose_json(ctx_with(&store, &entry, vec![], 0)).await;

        assert_eq!(v["stats"]["available"], true);
        assert_eq!(v["stats"]["source"], "graph");
        assert_eq!(v["stats"]["total_nodes"], 100);
        assert_eq!(v["modules"]["source"], "graph");
        assert_eq!(v["route_groups"]["source"], "graph");
        // Opt-in section is absent entirely, not available:false (D3).
        assert!(v.get("hotspots").is_none(), "hotspots must be opt-in");
        // No wiki listing → explicit degradation with a remedy, never silence.
        assert_eq!(v["wiki_pages"]["available"], false);
        assert!(v["wiki_pages"]["remedy"]
            .as_str()
            .unwrap()
            .contains("cih-engine wiki"));
        // Registry stats match the graph → no skew warning.
        assert_eq!(v["warnings"].as_array().unwrap().len(), 0);
        // No call-time timestamps anywhere in provenance (byte stability, D4).
        assert!(v["provenance"].get("as_of").is_none());
    }

    #[tokio::test]
    async fn module_rows_carry_anchor_symbols_with_canonical_ids() {
        let store = populated_store();
        let entry = entry("/nonexistent/demo", Some("/x"), 100);
        let v = compose_json(ctx_with(&store, &entry, vec!["modules".into()], 0)).await;

        let items = v["modules"]["items"].as_array().unwrap();
        assert_eq!(items[0]["name"], "loan"); // sorted by symbol_count desc
        assert_eq!(
            items[0]["anchor_symbols"].as_array().unwrap()[0],
            "Class:acme.LoanApi"
        );
        assert_eq!(
            items[1]["anchor_symbols"].as_array().unwrap()[0],
            "Class:acme.Ledger"
        );
    }

    #[tokio::test]
    async fn route_groups_bucket_generic_segments_and_emit_copyable_samples() {
        let store = populated_store();
        let entry = entry("/nonexistent/demo", Some("/x"), 100);
        let v = compose_json(ctx_with(&store, &entry, vec!["route_groups".into()], 0)).await;

        let items = v["route_groups"]["items"].as_array().unwrap();
        assert_eq!(items[0]["prefix"], "/api/v1/loans"); // 2 routes > /health's 1
        assert_eq!(items[0]["count"], 2);
        let sample = items[0]["samples"].as_array().unwrap()[0].as_str().unwrap();
        assert!(
            sample.starts_with("Route:POST /api/v1/loans"),
            "samples sort by (path, method) and lead with a trace_flow-ready Route id: {sample}"
        );
        assert!(
            sample.contains("→ Method:acme.LoanApi#"),
            "sample must carry the handler NodeId: {sample}"
        );
        assert_eq!(v["route_groups"]["total_routes"], 3);
    }

    #[tokio::test]
    async fn hotspots_only_when_requested() {
        let store = populated_store();
        let entry = entry("/nonexistent/demo", Some("/x"), 100);
        let v = compose_json(ctx_with(&store, &entry, vec!["hotspots".into()], 0)).await;

        assert_eq!(v["hotspots"]["available"], true);
        assert!(v.get("modules").is_none(), "unrequested sections stay out");
        assert_eq!(
            v["hotspots"]["items"].as_array().unwrap()[0]["id"],
            "Method:acme.LoanApi#big"
        );
    }

    #[tokio::test]
    async fn unknown_section_is_invalid_params() {
        let store = populated_store();
        let entry = entry("/nonexistent/demo", Some("/x"), 100);
        let err = match compose(ctx_with(&store, &entry, vec!["bogus".into()], 0)).await {
            Err(e) => e,
            Ok(_) => panic!("unknown section must be rejected"),
        };
        match err {
            AppError::InvalidInput { field, message } => {
                assert_eq!(field, "sections");
                assert!(message.contains("unknown section 'bogus'"));
                assert!(message.contains("stats"));
            }
            other => panic!("expected invalid sections error, got {other}"),
        }
    }

    #[tokio::test]
    async fn limit_caps_lists_and_marks_truncation_with_next_hint() {
        let mut store = populated_store();
        store.communities = (0..30)
            .map(|i| community(&format!("c{i:02}"), &format!("mod{i:02}"), 100 - i))
            .collect();
        let entry = entry("/nonexistent/demo", Some("/x"), 100);
        let v = compose_json(ctx_with(&store, &entry, vec!["modules".into()], 2)).await;

        assert_eq!(v["modules"]["items"].as_array().unwrap().len(), 2);
        assert_eq!(v["modules"]["total"], 30);
        assert_eq!(v["modules"]["truncated"], true);
        assert_eq!(v["modules"]["next"], "communities()");
    }

    #[tokio::test]
    async fn identical_state_serializes_to_identical_bytes() {
        let store = populated_store();
        let entry = entry("/nonexistent/demo", Some("/x"), 100);
        let a = compose(ctx_with(&store, &entry, vec![], 0)).await.unwrap();
        let b = compose(ctx_with(&store, &entry, vec![], 0)).await.unwrap();
        assert_eq!(
            serde_json::to_vec(&a).unwrap(),
            serde_json::to_vec(&b).unwrap(),
            "responses must be byte-stable for identical state"
        );
    }

    #[tokio::test]
    async fn discover_not_run_degrades_explicitly_never_as_empty_fact() {
        let store = FakeStore {
            kinds: vec![],
            total_nodes: 10,
            total_edges: 5,
            ..Default::default()
        };
        let entry = entry("/nonexistent/demo", None, 10);
        let v = compose_json(ctx_with(&store, &entry, vec![], 0)).await;

        assert_eq!(v["modules"]["available"], false);
        let reason = v["modules"]["reason"].as_str().unwrap();
        assert!(
            reason.contains("discover has not run"),
            "absence must be attributed to the pipeline, not the codebase: {reason}"
        );
        assert!(v["modules"]["remedy"]
            .as_str()
            .unwrap()
            .contains("cih-engine discover"));
        // Zero routes with no Route kind is a legitimate graph fact — available.
        assert_eq!(v["route_groups"]["available"], true);
        assert_eq!(v["route_groups"]["total_routes"], 0);
        // Sidecar absence with no discover artifacts → same attribution.
        assert!(v["entrypoints"]["scheduled"]["reason"]
            .as_str()
            .unwrap()
            .contains("discover has not run"));
    }

    #[tokio::test]
    async fn sidecar_absence_with_discover_artifacts_is_disambiguated() {
        let tmp = tempfile::tempdir().unwrap();
        let store = populated_store();
        let entry = entry(tmp.path().to_str().unwrap(), Some("/x"), 100);
        let v = compose_json(ctx_with(&store, &entry, vec!["entrypoints".into()], 0)).await;

        let reason = v["entrypoints"]["scheduled"]["reason"].as_str().unwrap();
        assert!(
            reason.contains("only when it detects"),
            "post-discover absence must not read as 'discover never ran': {reason}"
        );
    }

    #[tokio::test]
    async fn sidecar_items_parse_and_mtime_lands_in_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        let cih = tmp.path().join(".cih");
        std::fs::create_dir_all(&cih).unwrap();
        std::fs::write(
            cih.join("entrypoints.json"),
            r#"[{"method_id":"Method:acme.Jobs#nightly","kind":"Scheduled","topics":[]},
                {"method_id":"Method:acme.Ears#onLoan","kind":"EventListener","topics":["loan.created"]}]"#,
        )
        .unwrap();
        let store = populated_store();
        let entry = entry(tmp.path().to_str().unwrap(), Some("/x"), 100);
        let v = compose_json(ctx_with(&store, &entry, vec!["entrypoints".into()], 0)).await;

        let scheduled = &v["entrypoints"]["scheduled"];
        assert_eq!(scheduled["available"], true);
        assert_eq!(scheduled["source"], "artifact");
        assert_eq!(scheduled["total"], 2);
        // Deterministic order: kind, then method_id.
        assert_eq!(
            scheduled["items"][0]["id"], "Method:acme.Ears#onLoan",
            "EventListener sorts before Scheduled"
        );
        assert!(v["provenance"]["entrypoints_sidecar_mtime"]
            .as_str()
            .is_some());
    }

    #[tokio::test]
    async fn backend_error_after_first_query_degrades_that_section_only() {
        let mut store = populated_store();
        store.fail_communities = true;
        let entry = entry("/nonexistent/demo", Some("/x"), 100);
        let v = compose_json(ctx_with(&store, &entry, vec![], 0)).await;

        assert_eq!(v["modules"]["available"], false);
        let reason = v["modules"]["reason"].as_str().unwrap();
        assert!(
            reason.contains("graph query failed"),
            "an outage must not masquerade as 'discover never ran': {reason}"
        );
        // Sibling graph sections still serve.
        assert_eq!(v["route_groups"]["available"], true);
        assert_eq!(v["stats"]["available"], true);
    }

    #[tokio::test]
    async fn backend_error_on_first_query_is_a_hard_error() {
        let store = FakeStore {
            fail_summary: true,
            ..Default::default()
        };
        let entry = entry("/nonexistent/demo", Some("/x"), 100);
        assert!(
            compose(ctx_with(&store, &entry, vec![], 0)).await.is_err(),
            "graph_summary failure means the store is down — hard error"
        );
    }

    #[tokio::test]
    async fn byte_backstop_drops_whole_sections_in_declared_order() {
        let mut store = populated_store();
        // 15 modules × ~4KB names ≫ 32KB — only whole-section drops can save this.
        store.communities = (0..15)
            .map(|i| community(&format!("c{i:02}"), &"x".repeat(4096), 100 - i))
            .collect();
        let entry = entry("/nonexistent/demo", Some("/x"), 100);
        let v = compose_json(ctx_with(&store, &entry, vec![], 0)).await;

        assert!(v.get("modules").is_none(), "oversized section must drop");
        assert_eq!(v["stats"]["available"], true, "stats is never dropped");
        let warnings = v["warnings"].as_array().unwrap();
        let drop_warning = warnings
            .iter()
            .filter_map(|w| w.as_str())
            .find(|w| w.contains("dropped sections"))
            .expect("backstop must announce what it dropped");
        assert!(drop_warning.contains("modules"));
        assert!(drop_warning.contains("architecture_overview(sections="));
        let bytes = serde_json::to_vec(&serde_json::json!(v)).unwrap().len();
        assert!(
            bytes <= MAX_RESPONSE_BYTES,
            "response must fit the backstop: {bytes}"
        );
    }

    #[tokio::test]
    async fn skew_and_zero_stat_warnings_fire() {
        let store = populated_store(); // 100 live nodes
        let zero = entry("/nonexistent/demo", Some("/x"), 0);
        let v = compose_json(ctx_with(&store, &zero, vec!["stats".into()], 0)).await;
        assert!(v["warnings"].as_array().unwrap().iter().any(|w| w
            .as_str()
            .unwrap()
            .contains("registry stats for this repo are zero")));

        let skewed = entry("/nonexistent/demo", Some("/x"), 50);
        let v = compose_json(ctx_with(&store, &skewed, vec!["stats".into()], 0)).await;
        assert!(v["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("diverges from registry stats")));
    }

    #[tokio::test]
    async fn wiki_listing_produces_pointers_with_kind_priority() {
        let store = populated_store();
        let entry = entry("/nonexistent/demo", Some("/x"), 100);
        let page = |slug: &str, kind: &str| OverviewWikiPage {
            slug: slug.into(),
            title: format!("T {slug}"),
            kind: kind.into(),
        };
        let mut ctx = ctx_with(&store, &entry, vec!["wiki_pages".into()], 2);
        ctx.wiki = Some(OverviewWikiListing {
            pages: vec![
                page("dev/loan", "dev"),
                page("index", "index"),
                page("routes", "routes"),
            ],
            page_count: 3,
            source: "wiki-live",
            graph_version: None,
            generated_at: None,
        });
        let v = compose_json(ctx).await;

        assert_eq!(v["wiki_pages"]["source"], "wiki-live");
        let items = v["wiki_pages"]["items"].as_array().unwrap();
        assert_eq!(items[0]["slug"], "index", "index pages lead");
        assert_eq!(items[1]["slug"], "routes");
        assert_eq!(v["wiki_pages"]["truncated"], true);
        assert_eq!(v["wiki_pages"]["next"], "get_wiki_page(slug=\"index\")");
    }

    #[tokio::test]
    async fn group_block_serializes_with_member_stats() {
        let store = populated_store();
        let entry = entry("/nonexistent/demo", Some("/x"), 100);
        let mut ctx = ctx_with(&store, &entry, vec!["stats".into()], 0);
        ctx.groups = vec![GroupOut {
            name: "pack".into(),
            contracts_synced_at: Some("2026-07-01T00:00:00Z".into()),
            contracts_stale: true,
            members: vec![GroupMemberOut {
                repo: "demo".into(),
                nodes: 100,
                edges: 200,
                routes: 3,
                communities: 2,
                indexed_at: "2026-07-19T00:00:00Z".into(),
            }],
            next: Some("group_contracts(group=\"pack\")".into()),
        }];
        let v = compose_json(ctx).await;

        assert_eq!(v["group"]["source"], "registry");
        assert_eq!(v["group"]["groups"][0]["contracts_stale"], true);
        assert_eq!(v["group"]["groups"][0]["members"][0]["repo"], "demo");
    }

    #[test]
    fn route_prefix_groups_generic_segments() {
        assert_eq!(route_prefix("/api/v1/loans/{id}"), "/api/v1/loans");
        assert_eq!(route_prefix("/loans/{id}/repay"), "/loans");
        assert_eq!(route_prefix("/"), "/");
        assert_eq!(route_prefix("/api"), "/api");
        assert_eq!(route_prefix("/V2/savings"), "/V2/savings");
    }

    #[test]
    fn hints_reject_unsafe_values() {
        assert!(hint_safe("/api/v1/loans"));
        assert!(hint_safe("fineract-provider/index"));
        assert!(!hint_safe("bad\"quote"));
        assert!(!hint_safe("semi;colon"));
        assert!(!hint_safe(""));
    }

    /// End-to-end over the real embedded backend: artifacts → bulk_load →
    /// compose. Exercises the actual store read paths (graph_summary,
    /// communities, route_map, graph_overview, symbol_communities) instead of
    /// the fake — the design record's test-strategy item (b). Hermetic: tempdir
    /// DB, runs in the normal suite.
    #[tokio::test(flavor = "multi_thread")]
    async fn ladybug_end_to_end_materializes_real_sections() {
        use cih_core::{Edge, EdgeKind, GraphArtifacts, VersionId};

        let tmp = tempfile::tempdir().unwrap();
        let store = cih_ladybug::LadybugStore::connect(
            &tmp.path().join("db").to_string_lossy(),
            "overview_e2e",
        )
        .expect("connect embedded ladybug");

        let mut handler = node("Method:acme.Api#get/0", NodeKind::Method, "get");
        handler.file = "Api.java".into();
        let mut caller = node("Method:acme.Svc#run/0", NodeKind::Method, "run");
        caller.file = "Svc.java".into();
        let callee = node("Method:acme.Repo#load/0", NodeKind::Method, "load");
        let mut route = node("Route:GET /api/things", NodeKind::Route, "GET /api/things");
        route.props = Some(serde_json::json!({"path": "/api/things", "httpMethod": "GET"}));
        let mut comm_a = node("Community:acme.core", NodeKind::Community, "acme.core");
        comm_a.props = Some(serde_json::json!({"symbolCount": 2, "cohesion": 0.5}));
        let mut comm_b = node("Community:acme.data", NodeKind::Community, "acme.data");
        comm_b.props = Some(serde_json::json!({"symbolCount": 1, "cohesion": 0.25}));
        let nodes = vec![handler, caller, callee, route, comm_a, comm_b];
        let e = |src: &str, dst: &str, kind: EdgeKind| {
            Edge::new(
                NodeId::new(src.to_string()),
                NodeId::new(dst.to_string()),
                kind,
                1.0,
                "test".into(),
            )
        };
        let edges = vec![
            e(
                "Method:acme.Api#get/0",
                "Route:GET /api/things",
                EdgeKind::HandlesRoute,
            ),
            e(
                "Method:acme.Api#get/0",
                "Method:acme.Svc#run/0",
                EdgeKind::Calls,
            ),
            e(
                "Method:acme.Svc#run/0",
                "Method:acme.Repo#load/0",
                EdgeKind::Calls,
            ),
            e(
                "Method:acme.Api#get/0",
                "Community:acme.core",
                EdgeKind::MemberOf,
            ),
            e(
                "Method:acme.Svc#run/0",
                "Community:acme.core",
                EdgeKind::MemberOf,
            ),
            e(
                "Method:acme.Repo#load/0",
                "Community:acme.data",
                EdgeKind::MemberOf,
            ),
        ];
        let artifacts = GraphArtifacts::write(
            &tmp.path().join("artifacts"),
            VersionId::new("v1"),
            &nodes,
            &edges,
        )
        .expect("write artifacts");
        store.bulk_load(&artifacts).await.expect("bulk load");

        let entry = entry(tmp.path().to_str().unwrap(), Some("/x"), 6);
        let resp = compose(ComposeCtx {
            store: &store,
            entry: &entry,
            registry_stale: false,
            groups: vec![],
            wiki: None,
            wiki_warning: None,
            sections: vec![],
            limit: 0,
        })
        .await
        .expect("compose over ladybug");
        let v = serde_json::to_value(&resp).unwrap();

        assert_eq!(v["stats"]["source"], "graph");
        assert_eq!(v["stats"]["total_nodes"], 6);
        assert_eq!(v["modules"]["available"], true);
        let modules = v["modules"]["items"].as_array().unwrap();
        assert_eq!(modules[0]["name"], "acme.core", "largest cluster first");
        let anchors = modules[0]["anchor_symbols"].as_array().unwrap();
        assert!(
            !anchors.is_empty() && anchors[0].as_str().unwrap().starts_with("Method:"),
            "anchor symbols must be canonical NodeIds: {anchors:?}"
        );
        assert_eq!(v["route_groups"]["total_routes"], 1);
        assert_eq!(v["route_groups"]["items"][0]["prefix"], "/api/things");
        let sample = v["route_groups"]["items"][0]["samples"][0]
            .as_str()
            .unwrap();
        assert!(
            sample.starts_with("Route:GET /api/things → Method:acme.Api#get/0"),
            "sample must pair the Route id with its handler id: {sample}"
        );
        assert!(
            !v["entrypoints"]["hubs"].as_array().unwrap().is_empty(),
            "top-degree hubs must materialize from the real store"
        );
        assert!(v["entrypoints"]["scheduled"]["reason"]
            .as_str()
            .unwrap()
            .contains("only when it detects"));
    }

    /// Live-backend smoke against an indexed fineract graph — presence and
    /// labels, never exact counts (index-version-dependent, per the design
    /// record's validation rule). Run with:
    /// `cargo test -p cih-server fineract_overview -- --ignored`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs FalkorDB (FALKOR_URL or redis://127.0.0.1:6380) with an indexed 'fineract' graph"]
    async fn falkor_fineract_overview_presence_and_labels() {
        let url =
            std::env::var("FALKOR_URL").unwrap_or_else(|_| "redis://127.0.0.1:6380".to_string());
        let reg = cih_core::Registry::load();
        let entry = reg
            .find("fineract")
            .expect("fineract must be in the registry for this test")
            .clone();
        let store = cih_store_factory::connect_store(
            "falkor",
            &url,
            &entry.graph_key,
            &cih_store_factory::StoreOptions::default(),
        )
        .expect("connect falkor");

        let started = std::time::Instant::now();
        let resp = compose(ComposeCtx {
            store: store.as_ref(),
            entry: &entry,
            registry_stale: false,
            groups: vec![],
            wiki: None,
            wiki_warning: None,
            sections: vec![],
            limit: 0,
        })
        .await
        .expect("compose over live falkor");
        let elapsed = started.elapsed();
        let v = serde_json::to_value(&resp).unwrap();
        let bytes = serde_json::to_vec(&resp).unwrap().len();

        assert!(
            bytes <= MAX_RESPONSE_BYTES,
            "response must respect the byte cap: {bytes}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "live composition must stay interactive: {elapsed:?}"
        );
        assert_eq!(v["stats"]["source"], "graph");
        assert!(v["stats"]["total_nodes"].as_u64().unwrap() > 0);
        assert!(
            v["route_groups"]["total_routes"].as_u64().unwrap() >= 1,
            "fineract serves HTTP routes"
        );
        // Fix-D pinning: modules is either served or explicitly degraded with a
        // remedy — never absent, never a bare empty list.
        let modules = &v["modules"];
        if modules["available"] == true {
            assert!(!modules["items"].as_array().unwrap().is_empty());
        } else {
            assert!(modules["reason"].as_str().is_some());
        }
    }
}
