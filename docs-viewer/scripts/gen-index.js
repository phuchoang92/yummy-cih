#!/usr/bin/env node
// Generates src/pages/index.js before Docusaurus starts.
// Single-repo mode (CIH_WIKI_PATH): rich landing page reading manifest.json.
// Multi-repo mode (CIH_WIKI_REPOS_DIR): card grid listing all mounted repos.
'use strict';

const fs = require('fs');
const path = require('path');

const INDEX_OUT = path.join(__dirname, '..', 'src', 'pages', 'index.js');
fs.mkdirSync(path.dirname(INDEX_OUT), { recursive: true });

// Live-search backend, baked in at build time and fetched by the BROWSER —
// it must be reachable from the user's machine, not the container network.
const serverUrl = process.env.CIH_SERVER_URL || 'http://localhost:8080';

// ── Single-repo mode ──────────────────────────────────────────────────────────
if (process.env.CIH_WIKI_PATH) {
  const pagesDir = path.resolve(process.env.CIH_WIKI_PATH);
  const manifestPath = path.join(pagesDir, '..', 'manifest.json');

  let manifest = null;
  try { manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf-8')); } catch {}

  const repoName = manifest?.repo_name || process.env.CIH_REPO_NAME || 'Repository';
  const stats = manifest?.stats || {};
  const communities = manifest?.stats?.community_count ?? 0;
  const routes     = manifest?.stats?.route_count ?? 0;
  const classes    = manifest?.stats?.class_count ?? 0;
  const processes  = manifest?.stats?.process_count ?? 0;
  const llmProvider = manifest?.llm?.provider ?? null;

  // Build community cards from manifest.pages (only community-level po pages)
  const pages = Array.isArray(manifest?.pages) ? manifest.pages : [];
  // Use nav entries (features) for community cards — each feature = one card
  const navFeatures = Object.keys(manifest?.nav || {}).filter(k => k !== 'system');

  const communityCards = navFeatures.map(feature => ({
    name: feature.charAt(0).toUpperCase() + feature.slice(1),
    href: `/docs/${feature}/`,
  }));

  // Resolve persona button destinations: prefer system-level pages, fall back to first feature.
  const systemNavExists = (manifest?.nav || {})['system'] !== undefined;
  const firstFeature = navFeatures[0] || null;
  const poHref = systemNavExists ? '/docs/system/po' : (firstFeature ? `/docs/${firstFeature}/po` : '/docs/');
  const baHref = systemNavExists ? '/docs/system/ba' : (firstFeature ? `/docs/${firstFeature}/ba` : '/docs/');

  const cardsJs = communityCards.length > 0
    ? communityCards.map(c =>
        `    { name: ${JSON.stringify(c.name)}, href: ${JSON.stringify(c.href)} }`
      ).join(',\n')
    : '';

  const llmBadge = llmProvider
    ? `<span style={{ fontSize: '0.72rem', background: 'var(--cih-po-pill-bg)', color: 'var(--cih-po-text)', borderRadius: '999px', padding: '0.15rem 0.6rem', fontWeight: 700, marginLeft: '0.5rem' }}>AI enriched</span>`
    : '';

  const jsx = `// Auto-generated at startup by scripts/gen-index.js — do not edit
import React from 'react';
import Layout from '@theme/Layout';
import WikiSearch from '../components/WikiSearch';

// name is the registry repo name /wiki/search expects; routeBase is where
// this repo's docs are served.
const SEARCH = {
  serverUrl: ${JSON.stringify(serverUrl)},
  repos: [{ name: ${JSON.stringify(repoName)}, routeBase: '/docs' }],
};

const COMMUNITIES = [
${cardsJs}
];

function StatCard({ num, label }) {
  return (
    <div className="cih-stat-card">
      <div className="cih-stat-num">{num}</div>
      <div className="cih-stat-label">{label}</div>
    </div>
  );
}

function PersonaBtn({ href, cls, icon, label, desc }) {
  return (
    <a href={href} className={\`cih-persona-btn \${cls}\`}>
      <span style={{ fontSize: '1.1rem' }}>{icon}</span>
      <div>
        <div>{label}</div>
        <div style={{ fontWeight: 400, fontSize: '0.75rem', opacity: 0.75 }}>{desc}</div>
      </div>
    </a>
  );
}

function CommunityCard({ name, href }) {
  return (
    <a className="cih-community-card" href={href}>
      <div className="cih-card-name" title={name}>{name}</div>
    </a>
  );
}

export default function Home() {
  return (
    <Layout title="${repoName} — CIH Docs" description="Code Intelligence Hub documentation for ${repoName}">
      <main className="cih-hero">
        <h1>${repoName} ${llmBadge}</h1>
        <p className="cih-subtitle">Code Intelligence Hub — role-based documentation</p>

        <WikiSearch serverUrl={SEARCH.serverUrl} repos={SEARCH.repos} />

        <div className="cih-stats-row">
          <StatCard num={${communities}} label="Communities" />
          <StatCard num={${routes}} label="Routes" />
          <StatCard num={${classes}} label="Classes" />
          <StatCard num={${processes}} label="Processes" />
        </div>

        <div className="cih-section-title">Browse by persona</div>
        <div className="cih-persona-nav">
          <PersonaBtn href="${poHref}" cls="po" icon="👔" label="Product Owner" desc="Business capabilities &amp; stakeholder view" />
          <PersonaBtn href="${baHref}" cls="ba" icon="📊" label="Business Analyst" desc="Workflows, contracts &amp; event flows" />
          <PersonaBtn href="/docs/" cls="dev" icon="⚙️" label="Developer" desc="Technical structure, calls &amp; tests" />
        </div>

        {COMMUNITIES.length > 0 && (
          <>
            <div className="cih-section-title">Communities ({COMMUNITIES.length})</div>
            <div className="cih-community-grid">
              {COMMUNITIES.map(c => <CommunityCard key={c.href} name={c.name} href={c.href} />)}
            </div>
          </>
        )}
      </main>
    </Layout>
  );
}
`;

  fs.writeFileSync(INDEX_OUT, jsx, 'utf-8');
  console.log('[cih] single-repo mode: wrote landing page for', repoName);
  process.exit(0);
}

// ── Multi-repo mode ───────────────────────────────────────────────────────────
const reposDir = process.env.CIH_WIKI_REPOS_DIR
  ? path.resolve(process.env.CIH_WIKI_REPOS_DIR)
  : '/wiki';

let slugs = [];
try {
  slugs = fs.readdirSync(reposDir)
    .filter(f => { try { return fs.statSync(path.join(reposDir, f)).isDirectory(); } catch { return false; } })
    .sort();
} catch (e) {
  console.warn('[cih] cannot read repos dir:', reposDir, '-', e.message);
}

function readRepoMeta(slug) {
  try {
    const raw = fs.readFileSync(path.join(reposDir, slug, '..', 'manifest.json'), 'utf-8');
    const m = JSON.parse(raw);
    return {
      name: m.repo_name || slug,
      communities: m.stats?.community_count ?? 0,
      routes: m.stats?.route_count ?? 0,
      classes: m.stats?.class_count ?? 0,
    };
  } catch {}
  return { name: slug, communities: 0, routes: 0, classes: 0 };
}

const repos = slugs.map(slug => ({ slug, ...readRepoMeta(slug) }));
console.log('[cih] multi-repo mode:', repos.map(r => r.slug).join(', ') || '(no repos)');

const repoCardsJs = repos.length > 0
  ? repos.map(r =>
      `    { slug: ${JSON.stringify(r.slug)}, name: ${JSON.stringify(r.name)}, communities: ${r.communities}, routes: ${r.routes}, classes: ${r.classes} }`
    ).join(',\n')
  : '';

const emptyMsg = repos.length === 0
  ? `<p style={{ color: 'var(--ifm-color-secondary-darkest)' }}>
          No repositories found. Mount a wiki directory:
          <br /><code>-v /your/repo/.cih/wiki/pages:/wiki/my-repo:ro</code>
        </p>`
  : '';

const searchRepos = repos.map(r => ({ name: r.name, routeBase: `/${r.slug}` }));

const jsx = `// Auto-generated at startup by scripts/gen-index.js — do not edit
import React from 'react';
import Layout from '@theme/Layout';
import WikiSearch from '../components/WikiSearch';

// name is the registry repo name /wiki/search expects; routeBase is the
// mounted directory slug the docs are served under — they can differ.
const SEARCH = {
  serverUrl: ${JSON.stringify(serverUrl)},
  repos: ${JSON.stringify(searchRepos)},
};

const REPOS = [
${repoCardsJs}
];

function RepoCard({ slug, name, communities, routes, classes: cls }) {
  return (
    <a className="cih-repo-card" href={\`/\${slug}/\`}>
      <strong>{name}</strong>
      <small style={{ display: 'flex', gap: '0.75rem', marginTop: '0.4rem' }}>
        {communities > 0 && <span>{communities} communities</span>}
        {routes > 0 && <span>{routes} routes</span>}
        {cls > 0 && <span>{cls} classes</span>}
      </small>
    </a>
  );
}

export default function Home() {
  return (
    <Layout title="CIH Docs" description="Code Intelligence Hub — select a repository">
      <main className="cih-hero">
        <h1>Code Intelligence Hub</h1>
        <p className="cih-subtitle">Select a repository to explore its documentation.</p>
        ${emptyMsg ? emptyMsg : `
        <WikiSearch serverUrl={SEARCH.serverUrl} repos={SEARCH.repos} />
        <div className="cih-section-title">{REPOS.length} {REPOS.length === 1 ? 'repository' : 'repositories'}</div>
        <div className="cih-repo-grid">
          {REPOS.map(r => <RepoCard key={r.slug} {...r} />)}
        </div>`}
      </main>
    </Layout>
  );
}
`;

fs.writeFileSync(INDEX_OUT, jsx, 'utf-8');
console.log('[cih] wrote', INDEX_OUT);
