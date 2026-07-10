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
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::utils::resolve_repo;

pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 50;
/// Max characters of page text returned as a hit snippet.
pub const SNIPPET_MAX_CHARS: usize = 240;

/// The subset of `WikiManifest` the search endpoint needs. Parsed leniently
/// (`serde(default)`) so older manifests without newer fields still load.
#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default)]
    repo_name: String,
    #[serde(default)]
    graph_version: String,
    #[serde(default)]
    generated_at: String,
    #[serde(default)]
    pages: Vec<PageMeta>,
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
#[derive(Debug, Default)]
pub struct WikiFacets<'a> {
    /// Persona: `po`, `ba`, `dev`, ...
    pub role: Option<&'a str>,
    /// Page kind from the manifest: `feature`, `dev`, `index`, ...
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
            if facets.role.is_some_and(|r| !page.role.eq_ignore_ascii_case(r)) {
                continue;
            }
            if facets.kind.is_some_and(|k| !page.kind.eq_ignore_ascii_case(k)) {
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

/// Read a page's markdown body with front matter stripped. Returns `None` for
/// unreadable files or manifest paths that would escape `wiki_dir`.
fn read_page_body(wiki_dir: &Path, rel: &str) -> Option<String> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| matches!(c, Component::ParentDir))
    {
        return None;
    }
    let text = std::fs::read_to_string(wiki_dir.join(rel_path)).ok()?;
    Some(strip_front_matter(&text).to_string())
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
        if in_fence
            || trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with('|')
        {
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

#[derive(Clone)]
pub(crate) struct WikiSearchState {
    graph_key: String,
    cache: Arc<tokio::sync::RwLock<HashMap<PathBuf, Arc<WikiIndex>>>>,
}

impl WikiSearchState {
    pub(crate) fn new(graph_key: String) -> Self {
        Self {
            graph_key,
            cache: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        }
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
    let (repo_path, _) = match resolve_repo(&params.repo, &state.graph_key) {
        Ok(resolved) => resolved,
        Err(err) => return error_response(StatusCode::BAD_REQUEST, &err),
    };
    let wiki_dir = Path::new(&repo_path).join(".cih").join("wiki");
    if !wiki_dir.join("manifest.json").is_file() {
        return error_response(
            StatusCode::NOT_FOUND,
            &format!(
                "no generated wiki at {} — run `cih-engine wiki <repo>` first",
                wiki_dir.display()
            ),
        );
    }
    let index = match state.get_or_load(&wiki_dir).await {
        Ok(index) => index,
        Err(err) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to load wiki index: {err}"),
            )
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
