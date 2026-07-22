//! Wiki search, rendering, and cache repository implementation.
//!
//! One retrieval path for the docs viewer and agents: loads
//! `<repo>/.cih/wiki/manifest.json`, indexes each page's title + body with
//! [`TextIndex`], and serves faceted search (role, kind, feature). Indexes are
//! cached per wiki directory and reloaded when the manifest file changes, so a
//! `cih-engine wiki` regeneration is picked up without a server restart.

use std::collections::HashMap;
use std::io::BufRead;
use std::mem::size_of;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use async_trait::async_trait;
use cih_search::TextIndex;
use serde::{Deserialize, Serialize};

use crate::application::architecture_overview::{
    OverviewWikiListing, OverviewWikiPage, OverviewWikiRepository,
};
use crate::application::wiki_search::{
    WikiSearchDocument, WikiSearchFacets as AppWikiSearchFacets, WikiSearchHit,
    WikiSearchRepository,
};
use crate::domain::error::AppError;
use crate::domain::repository::ResolvedRepo;
use crate::infrastructure::cache::weighted::{AsyncCacheMetrics, AsyncWeightedCache};
use crate::ports::blocking_runtime::{blocking_timeout, run_blocking};
use crate::ports::retrieval_metrics::WikiRuntimeMetricsSnapshot;
use crate::ports::wiki_materialization_store::{MaterializedWikiPage, WikiMaterializationStore};

pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 50;
/// Max characters of page text returned as a hit snippet.
pub const SNIPPET_MAX_CHARS: usize = 240;

/// The subset of `WikiManifest` the search endpoint needs. Parsed leniently
/// (`serde(default)`) so older manifests without newer fields still load.
/// Also used by `resources.rs` for slug→path lookup.
#[derive(Debug, Deserialize)]
pub(crate) struct Manifest {
    #[serde(default)]
    pub(crate) repo_name: String,
    #[serde(default)]
    pub(crate) graph_version: String,
    #[serde(default)]
    pub(crate) generated_at: String,
    #[serde(default)]
    pub(crate) pages: Vec<PageMeta>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PageMeta {
    pub slug: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub kind: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub community_id: Option<String>,
}

/// A facet value that matches no page in this repo's wiki, with the values that
/// would have worked. Returning this instead of an empty result set is what
/// stops `kind="devs"` from looking like "no such documentation".
pub(crate) struct UnknownFacet {
    pub(crate) field: &'static str,
    pub(crate) value: String,
    pub(crate) available: Vec<String>,
}

/// Values are enumerated from the pages actually present, not a hard-coded
/// list: the wiki generator's kind/role sets evolve (personas, `routes`,
/// `api-flow`, `listener-flow`, …) and a stale allowlist here would reject
/// perfectly valid facets.
fn unknown_facet(pages: &[PageMeta], facets: &WikiFacets) -> Option<UnknownFacet> {
    fn distinct(values: impl Iterator<Item = String>) -> Vec<String> {
        let mut values: Vec<String> = values.collect();
        values.sort();
        values.dedup();
        values
    }

    if let Some(role) = facets.role {
        if !pages.iter().any(|p| p.role.eq_ignore_ascii_case(role)) {
            return Some(UnknownFacet {
                field: "role",
                value: role.to_string(),
                available: distinct(
                    pages
                        .iter()
                        .filter(|p| !p.role.is_empty())
                        .map(|p| p.role.clone()),
                ),
            });
        }
    }
    if let Some(kind) = facets.kind {
        if !pages.iter().any(|p| p.kind.eq_ignore_ascii_case(kind)) {
            return Some(UnknownFacet {
                field: "kind",
                value: kind.to_string(),
                available: distinct(
                    pages
                        .iter()
                        .filter(|p| !p.kind.is_empty())
                        .map(|p| p.kind.clone()),
                ),
            });
        }
    }
    if let Some(feature) = facets.feature {
        if !pages
            .iter()
            .any(|p| p.community_id.as_deref() == Some(feature))
        {
            return Some(UnknownFacet {
                field: "feature",
                value: feature.to_string(),
                // Feature ids are unbounded in principle; show a usable sample.
                available: distinct(pages.iter().filter_map(|p| p.community_id.clone()))
                    .into_iter()
                    .take(20)
                    .collect(),
            });
        }
    }
    None
}

impl UnknownFacet {
    pub(crate) fn into_app_error(self) -> AppError {
        let available = if self.available.is_empty() {
            "this wiki has no pages carrying that facet".to_string()
        } else {
            format!("available: {}", self.available.join(", "))
        };
        AppError::InvalidInput {
            field: match self.field {
                "role" => "role",
                "kind" => "kind",
                _ => "feature",
            },
            message: format!(
                "no wiki page has {} '{}' in this repo — {available}",
                self.field, self.value
            ),
        }
    }
}

/// An immutable, searchable snapshot of one generated wiki.
pub struct WikiIndex {
    wiki_dir: PathBuf,
    manifest_mtime: SystemTime,
    pub repo_name: String,
    pub graph_version: String,
    pub generated_at: String,
    pages: Vec<PageMeta>,
    page_by_slug: HashMap<String, usize>,
    index: TextIndex,
}

#[derive(Debug, Serialize)]
pub struct WikiHit {
    pub slug: String,
    pub role: String,
    pub title: String,
    pub kind: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community_id: Option<String>,
    pub score: f32,
    pub snippet: String,
}

/// Facet filters applied to ranked hits. `None` = no filter on that facet.
///
/// Field semantics follow the manifest's (historically confusing) naming:
/// `role` is the feature/module grouping a page belongs to, while `kind` is
/// the page type — and persona pages carry their persona AS the kind
/// (`po`, `ba`, `dev`), so persona filtering goes through `kind`.
#[derive(Debug, Default)]
pub struct WikiFacets<'a> {
    /// Feature/module grouping (manifest `role`): e.g. `loan`, `system`, `shared`.
    pub role: Option<&'a str>,
    /// Page kind: `po`, `ba`, `dev`, `index`, `routes`, `api-flow`, ...
    pub kind: Option<&'a str>,
    /// Matches the page's `community_id`.
    pub feature: Option<&'a str>,
}

/// Load and index the wiki under `wiki_dir` (must contain `manifest.json`).
pub fn load_wiki_index(wiki_dir: &Path) -> anyhow::Result<WikiIndex> {
    let manifest_path = wiki_dir.join("manifest.json");
    let manifest_mtime = std::fs::metadata(&manifest_path)?.modified()?;
    let manifest: Manifest = serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;

    // Corpus per page: manifest metadata + markdown body (front matter stripped).
    // Pages whose file is missing or unsafe still index on metadata alone, so
    // ordinals always line up with `pages`.
    let corpus: Vec<String> = manifest
        .pages
        .iter()
        .map(|page| {
            let body = read_page_body(wiki_dir, &page.path).unwrap_or_default();
            format!(
                "{} {} {} {} {}",
                page.title, page.role, page.kind, page.slug, body
            )
        })
        .collect();
    let index = TextIndex::build(corpus.iter().map(String::as_str));

    let page_by_slug = manifest
        .pages
        .iter()
        .enumerate()
        .map(|(ordinal, page)| (page.slug.clone(), ordinal))
        .collect();
    Ok(WikiIndex {
        wiki_dir: wiki_dir.to_path_buf(),
        manifest_mtime,
        repo_name: manifest.repo_name,
        graph_version: manifest.graph_version,
        generated_at: manifest.generated_at,
        pages: manifest.pages,
        page_by_slug,
        index,
    })
}

impl WikiIndex {
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    fn estimated_size_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.wiki_dir.capacity())
            .saturating_add(self.repo_name.capacity())
            .saturating_add(self.graph_version.capacity())
            .saturating_add(self.generated_at.capacity())
            .saturating_add(page_meta_weight(&self.pages, self.pages.capacity()))
            .saturating_add(
                self.page_by_slug.iter().fold(
                    self.page_by_slug
                        .capacity()
                        .saturating_mul(size_of::<(String, usize)>()),
                    |total, (slug, _)| total.saturating_add(slug.capacity()),
                ),
            )
            .saturating_add(self.index.estimated_size_bytes())
    }

    pub fn page_by_slug(&self, slug: &str) -> Option<&PageMeta> {
        self.page_by_slug
            .get(slug)
            .map(|ordinal| &self.pages[*ordinal])
    }

    /// A page's raw markdown, front matter included — the front matter carries
    /// provenance (enrichment tier, graph_version) callers should see.
    pub fn page_raw(&self, page: &PageMeta) -> Option<String> {
        read_page_raw(&self.wiki_dir, &page.path)
    }

    fn is_current(&self) -> bool {
        !self.wiki_dir.join(".publishing").exists()
            && std::fs::metadata(self.wiki_dir.join("manifest.json"))
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| modified == self.manifest_mtime)
    }

    /// Report a facet value that matches no page (see [`unknown_facet`]).
    pub(crate) fn unknown_facet(&self, facets: &WikiFacets) -> Option<UnknownFacet> {
        unknown_facet(&self.pages, facets)
    }

    /// Rank pages against `query`, best first, then apply facet filters and
    /// truncate to `limit`. Duplicate manifest entries (same slug) collapse to
    /// their best-ranked hit. Snippets are read from disk for surviving hits only.
    pub fn search(&self, query: &str, facets: &WikiFacets, limit: usize) -> Vec<WikiHit> {
        let ranked = self.index.search(query, self.pages.len());
        let mut seen_slugs = std::collections::HashSet::new();
        let mut hits = Vec::new();
        for (ordinal, score) in ranked {
            let page = &self.pages[ordinal];
            if !seen_slugs.insert(page.slug.as_str()) {
                continue;
            }
            if facets
                .role
                .is_some_and(|r| !page.role.eq_ignore_ascii_case(r))
            {
                continue;
            }
            if facets
                .kind
                .is_some_and(|k| !page.kind.eq_ignore_ascii_case(k))
            {
                continue;
            }
            if facets
                .feature
                .is_some_and(|f| page.community_id.as_deref() != Some(f))
            {
                continue;
            }
            let body = read_page_body(&self.wiki_dir, &page.path).unwrap_or_default();
            hits.push(WikiHit {
                slug: page.slug.clone(),
                role: page.role.clone(),
                title: page.title.clone(),
                kind: page.kind.clone(),
                path: page.path.clone(),
                community_id: page.community_id.clone(),
                score,
                snippet: make_snippet(&body, query, SNIPPET_MAX_CHARS),
            });
            if hits.len() == limit {
                break;
            }
        }
        hits
    }
}

/// Read a page's raw markdown (front matter included). Returns `None` for
/// unreadable files or manifest paths that would escape `wiki_dir`.
pub(crate) fn read_page_raw(wiki_dir: &Path, rel: &str) -> Option<String> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| matches!(c, Component::ParentDir))
    {
        return None;
    }
    std::fs::read_to_string(wiki_dir.join(rel_path)).ok()
}

/// Read a page's markdown body with front matter stripped.
fn read_page_body(wiki_dir: &Path, rel: &str) -> Option<String> {
    read_page_raw(wiki_dir, rel).map(|text| strip_front_matter(&text).to_string())
}

/// Strip a leading `---\n...\n---\n` YAML front matter block, if present.
pub fn strip_front_matter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---\n") else {
        return text;
    };
    match rest.find("\n---\n") {
        Some(pos) => &rest[pos + "\n---\n".len()..],
        None => text,
    }
}

/// First prose line containing a query token (falling back to the first prose
/// line), truncated to `max_chars`. Line-based so multibyte text never splits
/// mid-char. Headings, tables, and fenced code blocks (mermaid diagrams) are
/// skipped — they read as noise out of context.
pub fn make_snippet(body: &str, query: &str, max_chars: usize) -> String {
    let tokens = cih_search::tokenize(query);
    let mut fallback = None;
    let mut in_fence = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('|') {
            continue;
        }
        if fallback.is_none() {
            fallback = Some(trimmed);
        }
        let lower = trimmed.to_lowercase();
        if tokens.iter().any(|t| lower.contains(t.as_str())) {
            return truncate_chars(trimmed, max_chars);
        }
    }
    truncate_chars(fallback.unwrap_or(""), max_chars)
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars).collect();
    format!("{cut}…")
}

/// Errors from wiki lookup/search, mapped by the HTTP and MCP adapters.
pub(crate) enum WikiError {
    NotFound(String),
    Internal(String),
}

#[derive(Default)]
struct WikiRuntimeMetrics {
    manifest_overview_attempted: AtomicU64,
    manifest_overview_succeeded: AtomicU64,
    manifest_overview_failed: AtomicU64,
    live_build_attempted: AtomicU64,
    live_build_succeeded: AtomicU64,
    live_build_rejected_size: AtomicU64,
    live_build_failed: AtomicU64,
}

fn wiki_runtime_counters() -> &'static WikiRuntimeMetrics {
    static METRICS: OnceLock<WikiRuntimeMetrics> = OnceLock::new();
    METRICS.get_or_init(WikiRuntimeMetrics::default)
}

pub(crate) fn wiki_runtime_metrics() -> WikiRuntimeMetricsSnapshot {
    let metrics = wiki_runtime_counters();
    WikiRuntimeMetricsSnapshot {
        manifest_overview_attempted: metrics.manifest_overview_attempted.load(Ordering::Relaxed),
        manifest_overview_succeeded: metrics.manifest_overview_succeeded.load(Ordering::Relaxed),
        manifest_overview_failed: metrics.manifest_overview_failed.load(Ordering::Relaxed),
        live_build_attempted: metrics.live_build_attempted.load(Ordering::Relaxed),
        live_build_succeeded: metrics.live_build_succeeded.load(Ordering::Relaxed),
        live_build_rejected_size: metrics.live_build_rejected_size.load(Ordering::Relaxed),
        live_build_failed: metrics.live_build_failed.load(Ordering::Relaxed),
    }
}

/// Freshness token for the resident caches: the mtimes of everything a resident
/// `OwnedWiki` reads. Includes the enrichment caches (which change independently
/// of the graph artifacts), so adding/refreshing enrichment invalidates the
/// resident render + search without a server restart.
#[derive(Clone, PartialEq)]
struct Freshness {
    nodes: Option<SystemTime>,
    edges: Option<SystemTime>,
    class_enrich: Option<SystemTime>,
    wiki_meta: Option<SystemTime>,
}

impl Freshness {
    /// Compute from a repo path; `None` graph-artifacts mtime ⇒ no artifacts.
    fn probe(repo_path: &Path) -> Option<Self> {
        let ga = cih_core::GraphArtifacts::latest_in_dir(&repo_path.join(".cih").join("artifacts"))
            .ok()?;
        let mt = |p: PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        Some(Self {
            nodes: mt(ga.nodes_path),
            edges: mt(ga.edges_path),
            class_enrich: mt(repo_path.join(".cih").join("class-enrichment.json")),
            wiki_meta: mt(repo_path.join(".cih").join("wiki").join("wiki_meta.json")),
        })
    }

    fn has_complete_graph(&self) -> bool {
        self.nodes.is_some() && self.edges.is_some()
    }
}

/// A live BM25 search index built by rendering the resident wiki's pages
/// (no on-disk `.cih/wiki/` needed). Mirrors `WikiIndex` but holds bodies in
/// memory. Built once (renders all pages), cached + mtime-invalidated.
struct LiveWikiIndex {
    pages: Vec<PageMeta>,
    bodies: Vec<String>,
    index: TextIndex,
}

impl LiveWikiIndex {
    fn build(owned: &cih_wiki::OwnedWiki) -> Self {
        let mut pages = Vec::new();
        let mut bodies = Vec::new();
        let mut corpus = Vec::new();
        for (entry, content) in owned.rendered_manifest_pages() {
            let body = strip_front_matter(&content).to_string();
            corpus.push(format!(
                "{} {} {} {} {}",
                entry.title, entry.role, entry.kind, entry.slug, body
            ));
            pages.push(PageMeta {
                slug: entry.slug,
                role: entry.role,
                title: entry.title,
                kind: entry.kind,
                path: entry.path,
                community_id: entry.community_id,
            });
            bodies.push(body);
        }
        let index = TextIndex::build(corpus.iter().map(String::as_str));
        Self {
            pages,
            bodies,
            index,
        }
    }

    fn page_count(&self) -> usize {
        self.pages.len()
    }

    fn estimated_size_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(page_meta_weight(&self.pages, self.pages.capacity()))
            .saturating_add(self.bodies.iter().fold(
                self.bodies.capacity().saturating_mul(size_of::<String>()),
                |total, body| total.saturating_add(body.capacity()),
            ))
            .saturating_add(self.index.estimated_size_bytes())
    }

    fn unknown_facet(&self, facets: &WikiFacets) -> Option<UnknownFacet> {
        unknown_facet(&self.pages, facets)
    }

    /// Ranked, faceted search — mirrors `WikiIndex::search`, bodies from memory.
    fn search(&self, query: &str, facets: &WikiFacets, limit: usize) -> Vec<WikiHit> {
        let ranked = self.index.search(query, self.pages.len());
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut hits = Vec::new();
        for (ordinal, score) in ranked {
            let page = &self.pages[ordinal];
            if !seen.insert(page.slug.as_str()) {
                continue;
            }
            if facets
                .role
                .is_some_and(|r| !page.role.eq_ignore_ascii_case(r))
            {
                continue;
            }
            if facets
                .kind
                .is_some_and(|k| !page.kind.eq_ignore_ascii_case(k))
            {
                continue;
            }
            if facets
                .feature
                .is_some_and(|f| page.community_id.as_deref() != Some(f))
            {
                continue;
            }
            hits.push(WikiHit {
                slug: page.slug.clone(),
                role: page.role.clone(),
                title: page.title.clone(),
                kind: page.kind.clone(),
                path: page.path.clone(),
                community_id: page.community_id.clone(),
                score,
                snippet: make_snippet(&self.bodies[ordinal], query, SNIPPET_MAX_CHARS),
            });
            if hits.len() == limit {
                break;
            }
        }
        hits
    }
}

fn page_meta_weight(pages: &[PageMeta], capacity: usize) -> usize {
    pages.iter().fold(
        capacity.saturating_mul(size_of::<PageMeta>()),
        |total, page| {
            total
                .saturating_add(page.slug.capacity())
                .saturating_add(page.role.capacity())
                .saturating_add(page.title.capacity())
                .saturating_add(page.kind.capacity())
                .saturating_add(page.path.capacity())
                .saturating_add(page.community_id.as_ref().map_or(0, String::capacity))
        },
    )
}

type WikiGates = Arc<std::sync::Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>>;

fn gate_for(gates: &WikiGates, key: &Path) -> Arc<tokio::sync::Mutex<()>> {
    gates
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key.to_path_buf())
        .or_default()
        .clone()
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum WikiCacheKey {
    Disk(PathBuf),
    Resident(PathBuf),
    Live(PathBuf),
}

enum WikiCacheValue {
    Disk(Arc<WikiIndex>),
    Resident {
        freshness: Freshness,
        owned: Arc<cih_wiki::OwnedWiki>,
    },
    ResidentRejected {
        freshness: Freshness,
    },
    Live {
        freshness: Freshness,
        index: Arc<LiveWikiIndex>,
    },
}

enum ResidentCacheLookup {
    Miss,
    Available(Arc<cih_wiki::OwnedWiki>),
    Rejected,
}

#[derive(Clone)]
pub(crate) struct WikiSearchState {
    cache: Arc<AsyncWeightedCache<WikiCacheKey, WikiCacheValue>>,
    /// Single-flight gates for `get_or_load` - one per wiki dir. A std `Mutex`
    /// is fine: it's only held to clone the per-key async gate out (no `.await`).
    wiki_gates: WikiGates,
    /// Independent gates avoid duplicate resident loads and live search builds.
    /// Live-index construction may call `resident_for`, so sharing one gate
    /// between the two paths would deadlock.
    resident_gates: WikiGates,
    live_search_gates: WikiGates,
}

impl WikiSearchState {
    pub(crate) fn new() -> Self {
        Self::with_limits(
            cache_env("CIH_WIKI_CACHE_MAX_ENTRIES", 64),
            cache_env(
                "CIH_WIKI_CACHE_MAX_BYTES",
                crate::config::DEFAULT_WIKI_CACHE_MAX_BYTES,
            ),
        )
    }

    fn with_limits(max_entries: usize, max_weight_bytes: usize) -> Self {
        Self {
            cache: Arc::new(AsyncWeightedCache::new(max_entries, max_weight_bytes)),
            wiki_gates: Arc::new(std::sync::Mutex::new(HashMap::new())),
            resident_gates: Arc::new(std::sync::Mutex::new(HashMap::new())),
            live_search_gates: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    async fn cached_live(
        &self,
        repo_path: &Path,
        freshness: &Freshness,
    ) -> Option<Arc<LiveWikiIndex>> {
        let value = self
            .cache
            .get_if(&WikiCacheKey::Live(repo_path.to_path_buf()), |value| {
                matches!(
                    value,
                    WikiCacheValue::Live {
                        freshness: cached,
                        ..
                    } if cached == freshness && freshness.has_complete_graph()
                )
            })
            .await?;
        match value.as_ref() {
            WikiCacheValue::Live { index, .. } => Some(index.clone()),
            _ => None,
        }
    }

    async fn cached_resident(
        &self,
        repo_path: &Path,
        freshness: &Freshness,
    ) -> ResidentCacheLookup {
        let Some(value) = self
            .cache
            .get_if(&WikiCacheKey::Resident(repo_path.to_path_buf()), |value| {
                matches!(
                    value,
                    WikiCacheValue::Resident {
                        freshness: cached,
                        ..
                    } | WikiCacheValue::ResidentRejected {
                        freshness: cached,
                    } if cached == freshness && freshness.has_complete_graph()
                )
            })
            .await
        else {
            return ResidentCacheLookup::Miss;
        };
        match value.as_ref() {
            WikiCacheValue::Resident { owned, .. } => ResidentCacheLookup::Available(owned.clone()),
            WikiCacheValue::ResidentRejected { .. } => ResidentCacheLookup::Rejected,
            _ => ResidentCacheLookup::Miss,
        }
    }

    async fn retain(&self, key: WikiCacheKey, value: WikiCacheValue, weight_bytes: usize) {
        let result = self.cache.insert(key, Arc::new(value), weight_bytes).await;
        if !result.removed_keys.is_empty() {
            self.remove_gates(&result.removed_keys);
        }
        let metrics = self.metrics().await;
        tracing::debug!(
            retained = result.retained,
            weight_bytes,
            cache_hits = metrics.hits,
            cache_misses = metrics.misses,
            cache_builds = metrics.builds,
            cache_entries = metrics.retained_entries,
            cache_weight_bytes = metrics.retained_weight_bytes,
            cache_evictions = metrics.evictions,
            cache_oversize = metrics.oversize,
            "wiki cache updated"
        );
    }

    fn remove_gates(&self, keys: &[WikiCacheKey]) {
        for key in keys {
            let (gates, path) = match key {
                WikiCacheKey::Disk(path) => (&self.wiki_gates, path),
                WikiCacheKey::Resident(path) => (&self.resident_gates, path),
                WikiCacheKey::Live(path) => (&self.live_search_gates, path),
            };
            gates
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(path);
        }
    }

    pub(crate) async fn metrics(&self) -> AsyncCacheMetrics {
        self.cache.metrics().await
    }

    /// Resolve `repo` to its live search index, building it (renders all pages)
    /// and caching it on first use, mtime-invalidated. `Ok(None)` when the repo
    /// has no graph artifacts (caller falls back to the on-disk index).
    async fn live_index_for(
        &self,
        repo: &ResolvedRepo,
    ) -> Result<Option<Arc<LiveWikiIndex>>, WikiError> {
        let repo_path = repo.canonical_path.clone();
        let Some(freshness) = Freshness::probe(&repo_path) else {
            return Ok(None);
        };
        if let Some(index) = self.cached_live(&repo_path, &freshness).await {
            return Ok(Some(index));
        }
        let gate = gate_for(&self.live_search_gates, &repo_path);
        let _held = gate.lock().await;
        // The artifact may have changed, or another waiter may have completed
        // the build, while this request waited for the per-repository gate.
        let Some(freshness) = Freshness::probe(&repo_path) else {
            return Ok(None);
        };
        if let Some(index) = self.cached_live(&repo_path, &freshness).await {
            return Ok(Some(index));
        }
        let Some(owned) = self.resident_for(repo).await? else {
            return Ok(None);
        };
        let index = run_blocking(blocking_timeout(), "wiki live index build", move || {
            LiveWikiIndex::build(&owned)
        })
        .await
        .map_err(|e| WikiError::Internal(e.to_string()))?;
        let index = Arc::new(index);
        let weight = index.estimated_size_bytes();
        self.retain(
            WikiCacheKey::Live(repo_path),
            WikiCacheValue::Live {
                freshness,
                index: index.clone(),
            },
            weight,
        )
        .await;
        Ok(Some(index))
    }

    /// Resolve `repo` to its resident renderer, (re)loading when the graph
    /// artifacts change. `Ok(None)` when the repo has no graph artifacts (caller
    /// falls back to on-disk pages). Loading + rendering are CPU-bound, so this
    /// and `render_slug` run on `spawn_blocking`.
    pub(crate) async fn resident_for(
        &self,
        repo: &ResolvedRepo,
    ) -> Result<Option<Arc<cih_wiki::OwnedWiki>>, WikiError> {
        let repo_path = repo.canonical_path.clone();
        // No graph artifacts ⇒ nothing to render (caller falls back to on-disk).
        let Some(freshness) = Freshness::probe(&repo_path) else {
            return Ok(None);
        };
        match self.cached_resident(&repo_path, &freshness).await {
            ResidentCacheLookup::Available(owned) => return Ok(Some(owned)),
            ResidentCacheLookup::Rejected => return Ok(None),
            ResidentCacheLookup::Miss => {}
        }
        let gate = gate_for(&self.resident_gates, &repo_path);
        let _held = gate.lock().await;
        let Some(freshness) = Freshness::probe(&repo_path) else {
            return Ok(None);
        };
        match self.cached_resident(&repo_path, &freshness).await {
            ResidentCacheLookup::Available(owned) => return Ok(Some(owned)),
            ResidentCacheLookup::Rejected => return Ok(None),
            ResidentCacheLookup::Miss => {}
        }
        wiki_runtime_counters()
            .live_build_attempted
            .fetch_add(1, Ordering::Relaxed);
        let node_cap = live_wiki_node_cap().map_err(WikiError::Internal)?;
        let cap_repo = repo_path.clone();
        let exceeds_cap = run_blocking(blocking_timeout(), "wiki live node cap", move || {
            graph_exceeds_node_cap(&cap_repo, node_cap)
        })
        .await
        .map_err(|error| WikiError::Internal(error.to_string()))?;
        match exceeds_cap {
            Ok(false) => {}
            Ok(true) => {
                wiki_runtime_counters()
                    .live_build_rejected_size
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    node_cap,
                    "live wiki materialization skipped; use a generated wiki bundle"
                );
                self.retain(
                    WikiCacheKey::Resident(repo_path),
                    WikiCacheValue::ResidentRejected { freshness },
                    size_of::<Freshness>(),
                )
                .await;
                return Ok(None);
            }
            Err(error) => {
                wiki_runtime_counters()
                    .live_build_failed
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    error = %error,
                    "live wiki node count unavailable; falling back to generated wiki bundle"
                );
                return Ok(None);
            }
        }
        let load_repo = repo_path.clone();
        let name = repo_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo")
            .to_string();
        let owned = match run_blocking(blocking_timeout(), "wiki resident load", move || {
            cih_wiki::OwnedWiki::load_package_mode(&load_repo, name)
        })
        .await
        {
            Ok(Ok(owned)) => owned,
            Ok(Err(error)) => {
                wiki_runtime_counters()
                    .live_build_failed
                    .fetch_add(1, Ordering::Relaxed);
                return Err(WikiError::Internal(format!(
                    "failed to load resident wiki: {error}"
                )));
            }
            Err(error) => {
                wiki_runtime_counters()
                    .live_build_failed
                    .fetch_add(1, Ordering::Relaxed);
                return Err(WikiError::Internal(error.to_string()));
            }
        };
        let owned = Arc::new(owned);
        let weight = owned.estimated_size_bytes();
        self.retain(
            WikiCacheKey::Resident(repo_path),
            WikiCacheValue::Resident {
                freshness,
                owned: owned.clone(),
            },
            weight,
        )
        .await;
        wiki_runtime_counters()
            .live_build_succeeded
            .fetch_add(1, Ordering::Relaxed);
        Ok(Some(owned))
    }

    /// Return the cached wiki index for an already-resolved repository. The
    /// single entry point is shared by the Axum handler and MCP tools.
    pub(crate) async fn index_for(&self, repo: &ResolvedRepo) -> Result<Arc<WikiIndex>, WikiError> {
        let wiki_dir = repo.canonical_path.join(".cih").join("wiki");
        if !wiki_dir.join("manifest.json").is_file() {
            return Err(WikiError::NotFound(format!(
                "no generated wiki at {} — run `cih-engine wiki <repo>` first",
                wiki_dir.display()
            )));
        }
        self.get_or_load(&wiki_dir)
            .await
            .map_err(|err| WikiError::Internal(format!("failed to load wiki index: {err}")))
    }

    /// Return the cached index for `wiki_dir`, (re)loading when the manifest's
    /// mtime differs from the cached snapshot.
    async fn get_or_load(&self, wiki_dir: &Path) -> anyhow::Result<Arc<WikiIndex>> {
        anyhow::ensure!(
            !wiki_dir.join(".publishing").exists(),
            "wiki publication is in progress; retry shortly"
        );
        let manifest_mtime = std::fs::metadata(wiki_dir.join("manifest.json"))?.modified()?;
        if let Some(hit) = self.peek_index(wiki_dir, manifest_mtime).await {
            return Ok(hit);
        }
        // Single-flight: serialize concurrent first-time loads of the same wiki
        // dir behind a per-key gate, then re-check — a racing caller may have
        // loaded (and cached) while we waited for the gate.
        let gate = gate_for(&self.wiki_gates, wiki_dir);
        let _held = gate.lock().await;
        if let Some(hit) = self.peek_index(wiki_dir, manifest_mtime).await {
            return Ok(hit);
        }
        let dir = wiki_dir.to_path_buf();
        let loaded = Arc::new(
            run_blocking(blocking_timeout(), "wiki index load", move || {
                load_wiki_index(&dir)
            })
            .await??,
        );
        anyhow::ensure!(
            loaded.is_current(),
            "wiki changed while its index was loading; retry shortly"
        );
        let weight = loaded.estimated_size_bytes();
        self.retain(
            WikiCacheKey::Disk(wiki_dir.to_path_buf()),
            WikiCacheValue::Disk(loaded.clone()),
            weight,
        )
        .await;
        Ok(loaded)
    }

    /// Cached index for `wiki_dir` iff its manifest mtime still matches.
    async fn peek_index(
        &self,
        wiki_dir: &Path,
        manifest_mtime: SystemTime,
    ) -> Option<Arc<WikiIndex>> {
        let value = self
            .cache
            .get_if(&WikiCacheKey::Disk(wiki_dir.to_path_buf()), |value| {
                matches!(
                    value,
                    WikiCacheValue::Disk(index) if index.manifest_mtime == manifest_mtime
                )
            })
            .await?;
        match value.as_ref() {
            WikiCacheValue::Disk(index) => Some(index.clone()),
            _ => None,
        }
    }
}

fn cache_env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn live_wiki_node_cap() -> Result<usize, String> {
    static CAP: OnceLock<Result<usize, String>> = OnceLock::new();
    CAP.get_or_init(|| match std::env::var("CIH_WIKI_LIVE_MAX_NODES") {
        Ok(value) => value
            .parse::<usize>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| "CIH_WIKI_LIVE_MAX_NODES must be a positive integer".to_string()),
        Err(std::env::VarError::NotPresent) => Ok(100_000),
        Err(error) => Err(format!("cannot read CIH_WIKI_LIVE_MAX_NODES: {error}")),
    })
    .clone()
}

pub(crate) fn validate_live_wiki_config() -> anyhow::Result<()> {
    live_wiki_node_cap().map(|_| ()).map_err(anyhow::Error::msg)
}

fn graph_exceeds_node_cap(repo_path: &Path, cap: usize) -> std::io::Result<bool> {
    let artifacts =
        cih_core::GraphArtifacts::latest_in_dir(&repo_path.join(".cih").join("artifacts"))
            .map_err(|error| std::io::Error::other(error.to_string()))?;
    let file = std::fs::File::open(artifacts.nodes_path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    for _ in 0..=cap {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(Clone)]
pub(crate) struct WikiBundleSearchRepository {
    state: WikiSearchState,
}

impl WikiBundleSearchRepository {
    pub(crate) fn new(state: WikiSearchState) -> Self {
        Self { state }
    }
}

#[async_trait]
impl WikiSearchRepository for WikiBundleSearchRepository {
    async fn search(
        &self,
        repo: &ResolvedRepo,
        query: &str,
        facets: &AppWikiSearchFacets,
        limit: usize,
    ) -> Result<WikiSearchDocument, AppError> {
        let facets = WikiFacets {
            role: facets.role.as_deref(),
            kind: facets.kind.as_deref(),
            feature: facets.feature.as_deref(),
        };
        if let Some(live) = self
            .state
            .live_index_for(repo)
            .await
            .map_err(wiki_app_error)?
        {
            // A facet value no page carries is caller error, not an empty
            // result: fail with the values that would have worked.
            if let Some(unknown) = live.unknown_facet(&facets) {
                return Err(unknown.into_app_error());
            }
            let hits = live
                .search(query, &facets, limit)
                .into_iter()
                .map(app_search_hit)
                .collect();
            return Ok(WikiSearchDocument {
                repo: None,
                graph_version: None,
                generated_at: None,
                query: query.to_string(),
                page_count: live.page_count(),
                hits,
                source: Some("live"),
            });
        }
        let index = self
            .state
            .index_for(repo)
            .await
            .map_err(|error| match error {
                WikiError::NotFound(_) => AppError::NotFound {
                    entity: "wiki",
                    key: repo.registry_entry.name.clone(),
                },
                WikiError::Internal(message) => AppError::Unavailable {
                    dependency: "wiki search index",
                    message,
                    retryable: false,
                },
            })?;
        if let Some(unknown) = index.unknown_facet(&facets) {
            return Err(unknown.into_app_error());
        }
        let hits = index
            .search(query, &facets, limit)
            .into_iter()
            .map(app_search_hit)
            .collect();
        if !index.is_current() {
            return Err(AppError::Unavailable {
                dependency: "wiki publication",
                message: "wiki changed while search results were being read".into(),
                retryable: true,
            });
        }
        Ok(WikiSearchDocument {
            repo: Some(index.repo_name.clone()),
            graph_version: Some(index.graph_version.clone()),
            generated_at: Some(index.generated_at.clone()),
            query: query.to_string(),
            page_count: index.page_count(),
            hits,
            source: None,
        })
    }
}

fn app_search_hit(hit: WikiHit) -> WikiSearchHit {
    WikiSearchHit {
        slug: hit.slug,
        role: hit.role,
        title: hit.title,
        kind: hit.kind,
        path: hit.path,
        community_id: hit.community_id,
        score: hit.score,
        snippet: hit.snippet,
    }
}

#[derive(Clone, Default)]
pub(crate) struct WikiOverviewRepository;

impl WikiOverviewRepository {
    pub(crate) fn new() -> Self {
        Self
    }
}

fn overview_page(page: &PageMeta) -> OverviewWikiPage {
    OverviewWikiPage {
        slug: page.slug.clone(),
        title: page.title.clone(),
        kind: page.kind.clone(),
    }
}

fn overview_wiki_error(error: WikiError) -> AppError {
    let message = match error {
        WikiError::NotFound(message) | WikiError::Internal(message) => message,
    };
    AppError::Unavailable {
        dependency: "wiki repository",
        message,
        retryable: false,
    }
}

#[async_trait]
impl OverviewWikiRepository for WikiOverviewRepository {
    async fn list_pages(
        &self,
        repo: &ResolvedRepo,
    ) -> Result<Option<OverviewWikiListing>, AppError> {
        wiki_runtime_counters()
            .manifest_overview_attempted
            .fetch_add(1, Ordering::Relaxed);
        let wiki_dir = repo.canonical_path.join(".cih").join("wiki");
        let loaded = run_blocking(blocking_timeout(), "wiki overview manifest", move || {
            if wiki_dir.join(".publishing").exists() {
                return Err("wiki publication is in progress; retry shortly".to_string());
            }
            let bytes = match std::fs::read(wiki_dir.join("manifest.json")) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(format!("failed to read wiki manifest: {error}")),
            };
            let manifest: Manifest = serde_json::from_slice(&bytes)
                .map_err(|error| format!("failed to parse wiki manifest: {error}"))?;
            if wiki_dir.join(".publishing").exists() {
                return Err("wiki publication changed during manifest read; retry shortly".into());
            }
            Ok(Some(manifest))
        })
        .await;
        let manifest = match loaded {
            Ok(Ok(manifest)) => manifest,
            Ok(Err(message)) => {
                wiki_runtime_counters()
                    .manifest_overview_failed
                    .fetch_add(1, Ordering::Relaxed);
                return Err(overview_wiki_error(WikiError::Internal(message)));
            }
            Err(error) => {
                wiki_runtime_counters()
                    .manifest_overview_failed
                    .fetch_add(1, Ordering::Relaxed);
                return Err(AppError::Unavailable {
                    dependency: "wiki repository",
                    message: error.to_string(),
                    retryable: true,
                });
            }
        };
        wiki_runtime_counters()
            .manifest_overview_succeeded
            .fetch_add(1, Ordering::Relaxed);

        Ok(manifest.map(|manifest| OverviewWikiListing {
            page_count: manifest.pages.len(),
            pages: manifest.pages.iter().map(overview_page).collect(),
            source: "wiki-bundle",
            graph_version: Some(manifest.graph_version),
            generated_at: Some(manifest.generated_at),
        }))
    }
}

#[derive(Clone)]
pub(crate) struct WikiBundlePageRepository {
    state: WikiSearchState,
}

impl WikiBundlePageRepository {
    pub(crate) fn new(state: WikiSearchState) -> Self {
        Self { state }
    }
}

#[async_trait]
impl WikiMaterializationStore for WikiBundlePageRepository {
    async fn get_page(
        &self,
        repo: &ResolvedRepo,
        slug: &str,
    ) -> Result<MaterializedWikiPage, AppError> {
        if let Some(owned) = self
            .state
            .resident_for(repo)
            .await
            .map_err(wiki_app_error)?
        {
            let slug = slug.to_string();
            let render_slug = slug.clone();
            let rendered = run_blocking(blocking_timeout(), "wiki render", move || {
                owned.render_slug(&render_slug)
            })
            .await
            .map_err(blocking_app_error)?;
            if let Some(page) = rendered {
                return Ok(MaterializedWikiPage {
                    slug: slug.to_string(),
                    version: "resident".to_string(),
                    content: page.content,
                });
            }
        }

        let index = self.state.index_for(repo).await.map_err(wiki_app_error)?;
        let page = index.page_by_slug(slug).ok_or_else(|| AppError::NotFound {
            entity: "wiki page",
            key: slug.to_string(),
        })?;
        let content = index.page_raw(page).ok_or_else(|| AppError::Unavailable {
            dependency: "wiki page",
            message: format!(
                "wiki page '{slug}' exists in the manifest but its file is unreadable"
            ),
            retryable: false,
        })?;
        if !index.is_current() {
            return Err(AppError::Unavailable {
                dependency: "wiki publication",
                message: "wiki changed while the page was being read".into(),
                retryable: true,
            });
        }
        Ok(MaterializedWikiPage {
            slug: slug.to_string(),
            version: index.graph_version.clone(),
            content,
        })
    }
}

fn wiki_app_error(error: WikiError) -> AppError {
    match error {
        WikiError::NotFound(message) => AppError::NotFound {
            entity: "wiki",
            key: message,
        },
        WikiError::Internal(message) => AppError::Unavailable {
            dependency: "wiki repository",
            message,
            retryable: false,
        },
    }
}

fn blocking_app_error(error: crate::ports::blocking_runtime::BlockingError) -> AppError {
    AppError::Unavailable {
        dependency: "blocking runtime",
        message: error.to_string(),
        retryable: true,
    }
}

#[cfg(test)]
mod single_flight_tests {
    use super::*;
    use cih_core::{RegistryEntry, RegistryStats};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(flavor = "multi_thread")]
    async fn per_repository_gate_serializes_same_key_but_not_distinct_keys() {
        let gates: WikiGates = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let repo = PathBuf::from("/tmp/wiki-repo");
        assert!(Arc::ptr_eq(
            &gate_for(&gates, &repo),
            &gate_for(&gates, &repo)
        ));
        assert!(!Arc::ptr_eq(
            &gate_for(&gates, &repo),
            &gate_for(&gates, Path::new("/tmp/other-wiki-repo"))
        ));

        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let gate = gate_for(&gates, &repo);
            let active = active.clone();
            let max_active = max_active.clone();
            tasks.push(tokio::spawn(async move {
                let _held = gate.lock().await;
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_active.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(max_active.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn disk_and_live_indexes_share_one_weight_budget() {
        let state = WikiSearchState::with_limits(8, 100);
        let disk_path = PathBuf::from("/repo/.cih/wiki");
        let disk = Arc::new(WikiIndex {
            wiki_dir: disk_path.clone(),
            manifest_mtime: SystemTime::UNIX_EPOCH,
            repo_name: "repo".into(),
            graph_version: "v1".into(),
            generated_at: String::new(),
            pages: Vec::new(),
            page_by_slug: HashMap::new(),
            index: TextIndex::default(),
        });
        state
            .retain(
                WikiCacheKey::Disk(disk_path.clone()),
                WikiCacheValue::Disk(disk),
                60,
            )
            .await;
        let live_path = PathBuf::from("/repo");
        state
            .retain(
                WikiCacheKey::Live(live_path.clone()),
                WikiCacheValue::Live {
                    freshness: Freshness {
                        nodes: Some(SystemTime::UNIX_EPOCH),
                        edges: Some(SystemTime::UNIX_EPOCH),
                        class_enrich: None,
                        wiki_meta: None,
                    },
                    index: Arc::new(LiveWikiIndex {
                        pages: Vec::new(),
                        bodies: Vec::new(),
                        index: TextIndex::default(),
                    }),
                },
                60,
            )
            .await;

        assert!(state
            .cache
            .get_if(&WikiCacheKey::Disk(disk_path), |_| true)
            .await
            .is_none());
        assert!(state
            .cache
            .get_if(&WikiCacheKey::Live(live_path), |_| true)
            .await
            .is_some());
        let metrics = state.metrics().await;
        assert_eq!(metrics.retained_entries, 1);
        assert_eq!(metrics.retained_weight_bytes, 60);
        assert_eq!(metrics.evictions, 1);
    }

    #[tokio::test]
    async fn overview_listing_reads_manifest_without_page_bodies() {
        let temp = tempfile::tempdir().unwrap();
        let wiki_dir = temp.path().join(".cih/wiki");
        std::fs::create_dir_all(&wiki_dir).unwrap();
        std::fs::write(
            wiki_dir.join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "repo_name": "demo",
                "graph_version": "v1",
                "generated_at": "2026-07-22T00:00:00Z",
                "pages": [{
                    "slug": "overview",
                    "title": "Overview",
                    "kind": "index",
                    "path": "missing-page.md"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let repo = ResolvedRepo::from_entry(RegistryEntry {
            name: "demo".into(),
            path: temp.path().display().to_string(),
            graph_key: "demo".into(),
            artifacts_dir: String::new(),
            community_artifacts_dir: None,
            indexed_at: String::new(),
            last_git_head: None,
            stats: RegistryStats::default(),
        });

        let listing = WikiOverviewRepository::new()
            .list_pages(&repo)
            .await
            .expect("manifest metadata should load")
            .expect("manifest should produce a listing");

        assert_eq!(listing.page_count, 1);
        assert_eq!(listing.pages[0].slug, "overview");
        assert_eq!(listing.graph_version.as_deref(), Some("v1"));
    }

    #[test]
    fn live_wiki_node_cap_stops_counting_after_cap() {
        let temp = tempfile::tempdir().unwrap();
        let artifacts = temp.path().join(".cih/artifacts/v1");
        std::fs::create_dir_all(&artifacts).unwrap();
        std::fs::write(artifacts.join("nodes.jsonl"), b"one\ntwo\n").unwrap();
        std::fs::write(artifacts.join("edges.jsonl"), b"").unwrap();

        assert!(graph_exceeds_node_cap(temp.path(), 1).unwrap());
        assert!(!graph_exceeds_node_cap(temp.path(), 2).unwrap());
    }
}

#[cfg(test)]
mod facet_tests {
    use super::*;

    fn page(role: &str, kind: &str, community: Option<&str>) -> PageMeta {
        PageMeta {
            slug: format!("{role}/{kind}"),
            role: role.to_string(),
            title: String::new(),
            kind: kind.to_string(),
            path: String::new(),
            community_id: community.map(str::to_string),
        }
    }

    fn pages() -> Vec<PageMeta> {
        vec![
            page("loan", "dev", Some("c-1")),
            page("loan", "po", Some("c-1")),
            page("system", "routes", None),
        ]
    }

    fn facets<'a>(
        role: Option<&'a str>,
        kind: Option<&'a str>,
        feature: Option<&'a str>,
    ) -> WikiFacets<'a> {
        WikiFacets {
            role,
            kind,
            feature,
        }
    }

    #[test]
    fn known_facet_values_are_accepted_case_insensitively() {
        let pages = pages();
        assert!(unknown_facet(&pages, &facets(None, None, None)).is_none());
        assert!(unknown_facet(&pages, &facets(Some("loan"), Some("dev"), Some("c-1"))).is_none());
        // Roles/kinds match case-insensitively, as the search filter does.
        assert!(unknown_facet(&pages, &facets(Some("LOAN"), Some("Dev"), None)).is_none());
    }

    /// The gap this closes: `kind="devs"` used to return zero hits with no
    /// error, which reads as "this repo has no such documentation".
    #[test]
    fn unknown_kind_reports_the_values_that_would_work() {
        let pages = pages();
        let unknown = unknown_facet(&pages, &facets(None, Some("devs"), None))
            .expect("an unmatched kind must be reported");
        assert_eq!(unknown.field, "kind");
        assert_eq!(unknown.value, "devs");
        assert_eq!(unknown.available, vec!["dev", "po", "routes"]);

        let error = unknown.into_app_error();
        match error {
            AppError::InvalidInput { field, message } => {
                assert_eq!(field, "kind");
                assert!(message.contains("devs"), "{message}");
                assert!(message.contains("dev, po, routes"), "{message}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn unknown_role_and_feature_are_reported_too() {
        let pages = pages();
        let unknown = unknown_facet(&pages, &facets(Some("lending"), None, None)).unwrap();
        assert_eq!(unknown.field, "role");
        assert_eq!(unknown.available, vec!["loan", "system"]);

        let unknown = unknown_facet(&pages, &facets(None, None, Some("c-9"))).unwrap();
        assert_eq!(unknown.field, "feature");
        assert_eq!(unknown.available, vec!["c-1"]);
    }

    /// Role is checked before kind, so the first offending facet is the one
    /// reported — the caller fixes one thing at a time.
    #[test]
    fn the_first_offending_facet_is_reported() {
        let pages = pages();
        let unknown = unknown_facet(&pages, &facets(Some("nope"), Some("alsonope"), None)).unwrap();
        assert_eq!(unknown.field, "role");
    }

    #[test]
    fn a_wiki_without_the_facet_says_so_rather_than_listing_nothing() {
        let pages = vec![page("", "", None)];
        let unknown = unknown_facet(&pages, &facets(None, Some("dev"), None)).unwrap();
        assert!(unknown.available.is_empty());
        let AppError::InvalidInput { message, .. } = unknown.into_app_error() else {
            panic!("expected InvalidInput");
        };
        assert!(
            message.contains("no pages carrying that facet"),
            "{message}"
        );
    }
}
