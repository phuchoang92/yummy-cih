// Live wiki search against cih-server's /wiki/search endpoint (P2.3).
// Server URL and repo list are baked into the generated landing page by
// scripts/gen-index.js. Falls back to a hint pointing at the navbar's
// offline search index when the server is unreachable.
import React, { useEffect, useRef, useState } from 'react';

const DEBOUNCE_MS = 250;
const LIMIT = 10;

// Manifest slugs map 1:1 onto docs routes except index pages, which
// Docusaurus serves at their directory root.
function hitHref(routeBase, slug) {
  if (slug === 'index') return `${routeBase}/`;
  if (slug.endsWith('/index')) return `${routeBase}/${slug.slice(0, -'/index'.length)}/`;
  return `${routeBase}/${slug}`;
}

// Persona pages carry their persona as the page kind (po/ba/dev); other
// kinds (index, routes, api-flow, ...) get the neutral pill.
function kindClass(kind) {
  return ['po', 'ba', 'dev'].includes(kind) ? `cih-search-pill kind-${kind}` : 'cih-search-pill';
}

export default function WikiSearch({ serverUrl, repos }) {
  const [query, setQuery] = useState('');
  const [repoIdx, setRepoIdx] = useState(0);
  const [result, setResult] = useState(null);
  const [error, setError] = useState(false);
  const [loading, setLoading] = useState(false);
  const abortRef = useRef(null);

  const repo = repos[repoIdx] || repos[0];

  useEffect(() => {
    const q = query.trim();
    if (!q) {
      setResult(null);
      setError(false);
      return undefined;
    }
    const timer = setTimeout(async () => {
      if (abortRef.current) abortRef.current.abort();
      const controller = new AbortController();
      abortRef.current = controller;
      setLoading(true);
      try {
        const url =
          `${serverUrl}/wiki/search?q=${encodeURIComponent(q)}` +
          `&repo=${encodeURIComponent(repo.name)}&limit=${LIMIT}`;
        const resp = await fetch(url, { signal: controller.signal });
        if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
        setResult(await resp.json());
        setError(false);
      } catch (e) {
        if (e.name !== 'AbortError') {
          setResult(null);
          setError(true);
        }
      } finally {
        setLoading(false);
      }
    }, DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [query, repoIdx, serverUrl, repo.name]);

  return (
    <div className="cih-search-box">
      <div className="cih-search-inputs">
        <input
          type="search"
          className="cih-search-input"
          placeholder="Search the wiki — features, flows, classes, routes…"
          value={query}
          onChange={e => setQuery(e.target.value)}
          autoFocus
          aria-label="Search wiki"
        />
        {repos.length > 1 && (
          <select
            className="cih-search-repo"
            value={repoIdx}
            onChange={e => setRepoIdx(Number(e.target.value))}
            aria-label="Repository"
          >
            {repos.map((r, i) => (
              <option key={r.name} value={i}>{r.name}</option>
            ))}
          </select>
        )}
      </div>

      {error && (
        <div className="cih-search-note">
          Live search unavailable (is cih-server running at <code>{serverUrl}</code>?) —
          use the navbar search for the offline index.
        </div>
      )}

      {result && result.hits && (
        <div className="cih-search-results">
          {result.hits.length === 0 && !loading && (
            <div className="cih-search-note">No pages match “{query.trim()}”.</div>
          )}
          {result.hits.map(hit => (
            <a key={hit.slug} className="cih-search-hit" href={hitHref(repo.routeBase, hit.slug)}>
              <div className="cih-search-hit-head">
                <span className="cih-search-hit-title">{hit.title}</span>
                <span className={kindClass(hit.kind)}>{hit.kind}</span>
                {hit.role && <span className="cih-search-hit-role">{hit.role}</span>}
              </div>
              {hit.snippet && <div className="cih-search-hit-snippet">{hit.snippet}</div>}
            </a>
          ))}
          {result.hits.length > 0 && (
            <div className="cih-search-provenance">
              graph {result.graph_version} · generated {result.generated_at} ·{' '}
              {result.page_count} pages indexed
            </div>
          )}
        </div>
      )}
    </div>
  );
}
