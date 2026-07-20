//! `/wiki/search` — BM25 retrieval over generated wiki pages (P2.1).
//!
//! One retrieval path for the docs viewer and agents: loads
//! `<repo>/.cih/wiki/manifest.json`, indexes each page's title + body with
//! [`TextIndex`], and serves faceted search (role, kind, feature). Indexes are
//! cached per wiki directory and reloaded when the manifest file changes, so a
//! `cih-engine wiki` regeneration is picked up without a server restart.

use std::collections::HashMap;
use std::mem::size_of;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use cih_search::TextIndex;
use rmcp::{model::CallToolResult, ErrorData as McpError};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app_error::AppError;
use crate::application::architecture_overview::{
    OverviewWikiListing, OverviewWikiPage, OverviewWikiRepository,
};
use crate::args::{GetWikiPageArgs, SearchWikiArgs};
use crate::blocking::{blocking_timeout, run_blocking};
use crate::repo_context::{RepoContextProvider, RepoSelector, ResolvedRepo};
use crate::utils::{json_result, text_result};
use crate::weighted_cache::{AsyncCacheMetrics, AsyncWeightedCache};

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

/// An immutable, searchable snapshot of one generated wiki.
pub struct WikiIndex {
    wiki_dir: PathBuf,
    manifest_mtime: SystemTime,
    pub repo_name: String,
    pub graph_version: String,
    pub generated_at: String,
    pages: Vec<PageMeta>,
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

    Ok(WikiIndex {
        wiki_dir: wiki_dir.to_path_buf(),
        manifest_mtime,
        repo_name: manifest.repo_name,
        graph_version: manifest.graph_version,
        generated_at: manifest.generated_at,
        pages: manifest.pages,
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
            .saturating_add(self.index.estimated_size_bytes())
    }

    pub fn page_by_slug(&self, slug: &str) -> Option<&PageMeta> {
        self.pages.iter().find(|page| page.slug == slug)
    }

    /// A page's raw markdown, front matter included — the front matter carries
    /// provenance (enrichment tier, graph_version) callers should see.
    pub fn page_raw(&self, page: &PageMeta) -> Option<String> {
        read_page_raw(&self.wiki_dir, &page.path)
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

fn wiki_err_to_mcp(err: WikiError) -> McpError {
    match err {
        WikiError::NotFound(msg) => McpError::invalid_params(msg, None),
        WikiError::Internal(msg) => McpError::internal_error(msg, None),
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
    Live {
        freshness: Freshness,
        index: Arc<LiveWikiIndex>,
    },
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
    ) -> Option<Arc<cih_wiki::OwnedWiki>> {
        let value = self
            .cache
            .get_if(&WikiCacheKey::Resident(repo_path.to_path_buf()), |value| {
                matches!(
                    value,
                    WikiCacheValue::Resident {
                        freshness: cached,
                        ..
                    } if cached == freshness && freshness.has_complete_graph()
                )
            })
            .await?;
        match value.as_ref() {
            WikiCacheValue::Resident { owned, .. } => Some(owned.clone()),
            _ => None,
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
        if let Some(owned) = self.cached_resident(&repo_path, &freshness).await {
            return Ok(Some(owned));
        }
        let gate = gate_for(&self.resident_gates, &repo_path);
        let _held = gate.lock().await;
        let Some(freshness) = Freshness::probe(&repo_path) else {
            return Ok(None);
        };
        if let Some(owned) = self.cached_resident(&repo_path, &freshness).await {
            return Ok(Some(owned));
        }
        let load_repo = repo_path.clone();
        let name = repo_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo")
            .to_string();
        let owned = run_blocking(blocking_timeout(), "wiki resident load", move || {
            cih_wiki::OwnedWiki::load_package_mode(&load_repo, name)
        })
        .await
        .map_err(|e| WikiError::Internal(e.to_string()))?
        .map_err(|e| WikiError::Internal(format!("failed to load resident wiki: {e}")))?;
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

#[derive(Debug, Deserialize)]
pub(crate) struct WikiSearchParams {
    #[serde(default)]
    q: String,
    /// Repo name or path from the registry; empty = the server's active graph key.
    #[serde(default)]
    repo: String,
    role: Option<String>,
    kind: Option<String>,
    feature: Option<String>,
    limit: Option<usize>,
}

#[derive(Clone)]
struct WikiRouterState {
    wiki: WikiSearchState,
    repo_contexts: Arc<dyn RepoContextProvider>,
}

pub(crate) fn router(wiki: WikiSearchState, repo_contexts: Arc<dyn RepoContextProvider>) -> Router {
    Router::new()
        .route("/wiki/search", get(wiki_search_handler))
        .with_state(WikiRouterState {
            wiki,
            repo_contexts,
        })
}

async fn wiki_search_handler(
    State(state): State<WikiRouterState>,
    Query(params): Query<WikiSearchParams>,
) -> Response {
    let query = params.q.trim();
    if query.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "missing query parameter 'q'");
    }
    let repo = match state
        .repo_contexts
        .resolve_repo(RepoSelector::from_wire(&params.repo))
    {
        Ok(repo) => repo,
        Err(error) => return app_error_response(error),
    };
    let index = match state.wiki.index_for(&repo).await {
        Ok(index) => index,
        Err(WikiError::NotFound(msg)) => return error_response(StatusCode::NOT_FOUND, &msg),
        Err(WikiError::Internal(msg)) => {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, &msg)
        }
    };

    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let facets = WikiFacets {
        role: params.role.as_deref(),
        kind: params.kind.as_deref(),
        feature: params.feature.as_deref(),
    };
    let hits = index.search(query, &facets, limit);

    Json(json!({
        "repo": index.repo_name,
        "graph_version": index.graph_version,
        "generated_at": index.generated_at,
        "query": query,
        "page_count": index.page_count(),
        "hits": hits,
    }))
    .into_response()
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn app_error_response(error: AppError) -> Response {
    match error {
        AppError::InvalidInput { field, message } => error_response(
            StatusCode::BAD_REQUEST,
            &format!("invalid {field}: {message}"),
        ),
        AppError::NotFound { entity, key } => error_response(
            StatusCode::BAD_REQUEST,
            &format!("{entity} '{key}' not found"),
        ),
        AppError::Unavailable {
            dependency,
            message,
            retryable,
        } => {
            tracing::error!(
                dependency,
                error = %message,
                retryable,
                "wiki repository dependency unavailable"
            );
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!(
                    "{dependency} unavailable{}",
                    if retryable { "; retry shortly" } else { "" }
                ),
            )
        }
    }
}

/// MCP tool body for `search_wiki`.
///
/// P3.8: searches a **live** index built from the resident graph (no
/// `.cih/wiki/` files needed, always fresh). Falls back to the on-disk bundle
/// when the repo has no graph artifacts.
pub(crate) async fn search_wiki(
    state: &WikiSearchState,
    repo: &ResolvedRepo,
    args: SearchWikiArgs,
) -> Result<CallToolResult, McpError> {
    let query = args.query.trim();
    if query.is_empty() {
        return Err(McpError::invalid_params("query must not be empty", None));
    }
    let limit = if args.limit == 0 {
        DEFAULT_LIMIT
    } else {
        args.limit
    }
    .clamp(1, MAX_LIMIT);
    fn non_empty(s: &str) -> Option<&str> {
        (!s.is_empty()).then_some(s)
    }
    let facets = WikiFacets {
        role: non_empty(&args.role),
        kind: non_empty(&args.kind),
        feature: non_empty(&args.feature),
    };

    // Live index first.
    if let Some(live) = state.live_index_for(repo).await.map_err(wiki_err_to_mcp)? {
        let hits = live.search(query, &facets, limit);
        return json_result(&json!({
            "query": query,
            "page_count": live.page_count(),
            "hits": hits,
            "source": "live",
        }));
    }

    // Fallback: on-disk generated bundle.
    let index = state.index_for(repo).await.map_err(wiki_err_to_mcp)?;
    let hits = index.search(query, &facets, limit);
    json_result(&json!({
        "repo": index.repo_name,
        "graph_version": index.graph_version,
        "generated_at": index.generated_at,
        "query": query,
        "page_count": index.page_count(),
        "hits": hits,
    }))
}

#[derive(Clone)]
pub(crate) struct WikiOverviewRepository {
    state: WikiSearchState,
}

impl WikiOverviewRepository {
    pub(crate) fn new(state: WikiSearchState) -> Self {
        Self { state }
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
        match self.state.live_index_for(repo).await {
            Ok(Some(live)) => {
                return Ok(Some(OverviewWikiListing {
                    pages: live.pages.iter().map(overview_page).collect(),
                    page_count: live.page_count(),
                    source: "wiki-live",
                    graph_version: None,
                    generated_at: None,
                }));
            }
            Ok(None) => {}
            Err(error) => return Err(overview_wiki_error(error)),
        }
        match self.state.index_for(repo).await {
            Ok(index) => Ok(Some(OverviewWikiListing {
                pages: index.pages.iter().map(overview_page).collect(),
                page_count: index.page_count(),
                source: "wiki-bundle",
                graph_version: Some(index.graph_version.clone()),
                generated_at: Some(index.generated_at.clone()),
            })),
            Err(WikiError::NotFound(_)) => Ok(None),
            Err(error) => Err(overview_wiki_error(error)),
        }
    }
}

/// MCP tool body for `get_wiki_page` — full page markdown by slug.
///
/// P3.8: renders the page **live** from the resident graph (always fresh at the
/// current graph_version, no `.cih/wiki/` files needed). Falls back to the
/// on-disk generated bundle when the repo has no graph artifacts or the slug
/// isn't a live page.
pub(crate) async fn get_wiki_page(
    state: &WikiSearchState,
    repo: &ResolvedRepo,
    args: GetWikiPageArgs,
) -> Result<CallToolResult, McpError> {
    // Live render first.
    if let Some(owned) = state.resident_for(repo).await.map_err(wiki_err_to_mcp)? {
        let slug = args.slug.clone();
        let rendered = run_blocking(blocking_timeout(), "wiki render", move || {
            owned.render_slug(&slug)
        })
        .await?;
        if let Some(page) = rendered {
            return text_result(page.content);
        }
        // Slug not a live page → fall through to on-disk (e.g. legacy bundle).
    }

    let index = state.index_for(repo).await.map_err(wiki_err_to_mcp)?;
    let page = index.page_by_slug(&args.slug).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "no wiki page with slug '{}' — use search_wiki to find slugs",
                args.slug
            ),
            None,
        )
    })?;
    let markdown = index.page_raw(page).ok_or_else(|| {
        McpError::internal_error(
            format!(
                "wiki page '{}' exists in the manifest but its file is unreadable",
                args.slug
            ),
            None,
        )
    })?;
    text_result(markdown)
}

#[cfg(test)]
mod single_flight_tests {
    use super::*;
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
}
