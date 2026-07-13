//! `/wiki/search` — BM25 retrieval over generated wiki pages (P2.1).
//!
//! One retrieval path for the docs viewer and agents: loads
//! `<repo>/.cih/wiki/manifest.json`, indexes each page's title + body with
//! [`TextIndex`], and serves faceted search (role, kind, feature). Indexes are
//! cached per wiki directory and reloaded when the manifest file changes, so a
//! `cih-engine wiki` regeneration is picked up without a server restart.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

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

use crate::args::{GetWikiPageArgs, SearchWikiArgs};
use crate::utils::{json_result, resolve_repo, text_result};

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

/// Errors from wiki lookup/search, mapped to HTTP statuses by the axum handler
/// and to `McpError`s by the MCP tools.
pub(crate) enum WikiError {
    BadRequest(String),
    NotFound(String),
    Internal(String),
}

fn wiki_err_to_mcp(err: WikiError) -> McpError {
    match err {
        WikiError::BadRequest(msg) | WikiError::NotFound(msg) => {
            McpError::invalid_params(msg, None)
        }
        WikiError::Internal(msg) => McpError::internal_error(msg, None),
    }
}

/// A resident renderer + the artifacts mtime it was built from (for invalidation).
struct ResidentEntry {
    nodes_mtime: Option<SystemTime>,
    owned: Arc<cih_wiki::OwnedWiki>,
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

/// A live search index + the artifacts mtime it was built from.
struct LiveSearchEntry {
    nodes_mtime: Option<SystemTime>,
    index: Arc<LiveWikiIndex>,
}

#[derive(Clone)]
pub(crate) struct WikiSearchState {
    graph_key: String,
    cache: Arc<tokio::sync::RwLock<HashMap<PathBuf, Arc<WikiIndex>>>>,
    /// Resident renderers for live on-demand page rendering (P3.8), keyed by
    /// repo path, invalidated on `.cih/artifacts` nodes.jsonl mtime change.
    render_cache: Arc<tokio::sync::RwLock<HashMap<PathBuf, ResidentEntry>>>,
    /// Live search indexes (built by rendering all resident pages), same keying.
    live_search_cache: Arc<tokio::sync::RwLock<HashMap<PathBuf, LiveSearchEntry>>>,
}

impl WikiSearchState {
    pub(crate) fn new(graph_key: String) -> Self {
        Self {
            graph_key,
            cache: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            render_cache: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            live_search_cache: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        }
    }

    /// Resolve `repo` to its live search index, building it (renders all pages)
    /// and caching it on first use, mtime-invalidated. `Ok(None)` when the repo
    /// has no graph artifacts (caller falls back to the on-disk index).
    async fn live_index_for(&self, repo: &str) -> Result<Option<Arc<LiveWikiIndex>>, WikiError> {
        let (repo_path, _) = resolve_repo(repo, &self.graph_key).map_err(WikiError::BadRequest)?;
        let repo_path = PathBuf::from(repo_path);
        let Ok(ga) =
            cih_core::GraphArtifacts::latest_in_dir(&repo_path.join(".cih").join("artifacts"))
        else {
            return Ok(None);
        };
        let mtime = std::fs::metadata(&ga.nodes_path)
            .and_then(|m| m.modified())
            .ok();
        if let Some(entry) = self.live_search_cache.read().await.get(&repo_path) {
            if entry.nodes_mtime == mtime && mtime.is_some() {
                return Ok(Some(entry.index.clone()));
            }
        }
        let Some(owned) = self.resident_for(repo).await? else {
            return Ok(None);
        };
        let index = tokio::task::spawn_blocking(move || LiveWikiIndex::build(&owned))
            .await
            .map_err(|e| WikiError::Internal(format!("live search build task failed: {e}")))?;
        let index = Arc::new(index);
        self.live_search_cache.write().await.insert(
            repo_path,
            LiveSearchEntry {
                nodes_mtime: mtime,
                index: index.clone(),
            },
        );
        Ok(Some(index))
    }

    /// Resolve `repo` to its resident renderer, (re)loading when the graph
    /// artifacts change. `Ok(None)` when the repo has no graph artifacts (caller
    /// falls back to on-disk pages). Loading + rendering are CPU-bound, so this
    /// and `render_slug` run on `spawn_blocking`.
    pub(crate) async fn resident_for(
        &self,
        repo: &str,
    ) -> Result<Option<Arc<cih_wiki::OwnedWiki>>, WikiError> {
        let (repo_path, _) = resolve_repo(repo, &self.graph_key).map_err(WikiError::BadRequest)?;
        let repo_path = PathBuf::from(repo_path);
        let artifacts = repo_path.join(".cih").join("artifacts");
        // Latest nodes.jsonl mtime = cache key freshness; absent ⇒ no artifacts.
        let Ok(ga) = cih_core::GraphArtifacts::latest_in_dir(&artifacts) else {
            return Ok(None);
        };
        let mtime = std::fs::metadata(&ga.nodes_path)
            .and_then(|m| m.modified())
            .ok();
        if let Some(entry) = self.render_cache.read().await.get(&repo_path) {
            if entry.nodes_mtime == mtime && mtime.is_some() {
                return Ok(Some(entry.owned.clone()));
            }
        }
        let load_repo = repo_path.clone();
        let name = repo_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo")
            .to_string();
        let owned = tokio::task::spawn_blocking(move || {
            cih_wiki::OwnedWiki::load_package_mode(&load_repo, name)
        })
        .await
        .map_err(|e| WikiError::Internal(format!("resident load task failed: {e}")))?
        .map_err(|e| WikiError::Internal(format!("failed to load resident wiki: {e}")))?;
        let owned = Arc::new(owned);
        self.render_cache.write().await.insert(
            repo_path,
            ResidentEntry {
                nodes_mtime: mtime,
                owned: owned.clone(),
            },
        );
        Ok(Some(owned))
    }

    /// Resolve `repo` (registry name/path, or empty for the active graph key)
    /// to its cached wiki index. The single entry point shared by the axum
    /// handler and the MCP tools.
    pub(crate) async fn index_for(&self, repo: &str) -> Result<Arc<WikiIndex>, WikiError> {
        let (repo_path, _) = resolve_repo(repo, &self.graph_key).map_err(WikiError::BadRequest)?;
        let wiki_dir = Path::new(&repo_path).join(".cih").join("wiki");
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
        if let Some(cached) = self.cache.read().await.get(wiki_dir) {
            if cached.manifest_mtime == manifest_mtime {
                return Ok(cached.clone());
            }
        }
        let dir = wiki_dir.to_path_buf();
        let loaded = tokio::task::spawn_blocking(move || load_wiki_index(&dir)).await??;
        let loaded = Arc::new(loaded);
        self.cache
            .write()
            .await
            .insert(wiki_dir.to_path_buf(), loaded.clone());
        Ok(loaded)
    }
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

pub(crate) fn router(state: WikiSearchState) -> Router {
    Router::new()
        .route("/wiki/search", get(wiki_search_handler))
        .with_state(state)
}

async fn wiki_search_handler(
    State(state): State<WikiSearchState>,
    Query(params): Query<WikiSearchParams>,
) -> Response {
    let query = params.q.trim();
    if query.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "missing query parameter 'q'");
    }
    let index = match state.index_for(&params.repo).await {
        Ok(index) => index,
        Err(WikiError::BadRequest(msg)) => return error_response(StatusCode::BAD_REQUEST, &msg),
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

/// MCP tool body for `search_wiki`.
///
/// P3.8: searches a **live** index built from the resident graph (no
/// `.cih/wiki/` files needed, always fresh). Falls back to the on-disk bundle
/// when the repo has no graph artifacts.
pub(crate) async fn search_wiki(
    state: &WikiSearchState,
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
    if let Some(live) = state
        .live_index_for(&args.repo)
        .await
        .map_err(wiki_err_to_mcp)?
    {
        let hits = live.search(query, &facets, limit);
        return json_result(&json!({
            "query": query,
            "page_count": live.page_count(),
            "hits": hits,
            "source": "live",
        }));
    }

    // Fallback: on-disk generated bundle.
    let index = state.index_for(&args.repo).await.map_err(wiki_err_to_mcp)?;
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

/// MCP tool body for `get_wiki_page` — full page markdown by slug.
///
/// P3.8: renders the page **live** from the resident graph (always fresh at the
/// current graph_version, no `.cih/wiki/` files needed). Falls back to the
/// on-disk generated bundle when the repo has no graph artifacts or the slug
/// isn't a live page.
pub(crate) async fn get_wiki_page(
    state: &WikiSearchState,
    args: GetWikiPageArgs,
) -> Result<CallToolResult, McpError> {
    // Live render first.
    if let Some(owned) = state
        .resident_for(&args.repo)
        .await
        .map_err(wiki_err_to_mcp)?
    {
        let slug = args.slug.clone();
        let rendered = tokio::task::spawn_blocking(move || owned.render_slug(&slug))
            .await
            .map_err(|e| McpError::internal_error(format!("render task failed: {e}"), None))?;
        if let Some(page) = rendered {
            return text_result(page.content);
        }
        // Slug not a live page → fall through to on-disk (e.g. legacy bundle).
    }

    let index = state.index_for(&args.repo).await.map_err(wiki_err_to_mcp)?;
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
