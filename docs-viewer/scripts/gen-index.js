#!/usr/bin/env node
// Generates src/pages/index.js before Docusaurus starts.
// In single-repo mode (CIH_WIKI_PATH set): removes any stale index page.
// In multi-repo mode: writes a homepage that lists all mounted repos.
'use strict';

const fs = require('fs');
const path = require('path');

const INDEX_OUT = path.join(__dirname, '..', 'src', 'pages', 'index.js');

// ── Single-repo mode ─────────────────────────────────────────────────────────
if (process.env.CIH_WIKI_PATH) {
  if (fs.existsSync(INDEX_OUT)) fs.unlinkSync(INDEX_OUT);
  console.log('[cih] single-repo mode: CIH_WIKI_PATH=' + process.env.CIH_WIKI_PATH);
  process.exit(0);
}

// ── Multi-repo mode ───────────────────────────────────────────────────────────
const reposDir = process.env.CIH_WIKI_REPOS_DIR
  ? path.resolve(process.env.CIH_WIKI_REPOS_DIR)
  : '/wiki';

let slugs = [];
try {
  slugs = fs.readdirSync(reposDir)
    .filter(f => {
      try { return fs.statSync(path.join(reposDir, f)).isDirectory(); } catch { return false; }
    })
    .sort();
} catch (e) {
  console.warn('[cih] cannot read repos dir:', reposDir, '-', e.message);
}

function readDisplayName(slug) {
  try {
    const raw = fs.readFileSync(path.join(reposDir, slug, '..', 'manifest.json'), 'utf-8');
    const m = JSON.parse(raw);
    if (m.repo_name) return m.repo_name;
  } catch {}
  return slug;
}

const repos = slugs.map(slug => ({ slug, name: readDisplayName(slug) }));
console.log('[cih] multi-repo mode:', repos.map(r => r.slug).join(', ') || '(no repos mounted)');

const cards = repos.length > 0
  ? repos.map(({ slug, name }) => `      <RepoCard slug="${slug}" name="${name}" />`).join('\n')
  : `      <p style={{ color: 'var(--ifm-color-secondary-darkest)' }}>
        No repositories found. Mount a wiki directory into the container:
        <br />
        <code>-v /your/repo/.cih/wiki/pages:/wiki/my-repo:ro</code>
      </p>`;

const jsx = `// Auto-generated at startup by scripts/gen-index.js — do not edit
import React from 'react';
import Layout from '@theme/Layout';

function RepoCard({ slug, name }) {
  return (
    <a
      href={\`/\${slug}/\`}
      style={{
        display: 'block',
        padding: '1rem 1.5rem',
        marginBottom: '0.75rem',
        border: '1px solid var(--ifm-color-emphasis-300)',
        borderRadius: '8px',
        textDecoration: 'none',
        color: 'inherit',
        maxWidth: '480px',
        transition: 'border-color 0.15s',
      }}
      onMouseEnter={e => e.currentTarget.style.borderColor = 'var(--ifm-color-primary)'}
      onMouseLeave={e => e.currentTarget.style.borderColor = 'var(--ifm-color-emphasis-300)'}
    >
      <strong style={{ fontSize: '1.05rem' }}>{name}</strong>
      <br />
      <small style={{ color: 'var(--ifm-color-secondary-darkest)' }}>/{slug}/</small>
    </a>
  );
}

export default function Home() {
  return (
    <Layout title="CIH Docs" description="Code Intelligence Hub — select a repository">
      <main style={{ padding: '3rem 2rem', maxWidth: '720px', margin: '0 auto' }}>
        <h1 style={{ marginBottom: '0.25rem' }}>Code Intelligence Hub</h1>
        <p style={{ color: 'var(--ifm-color-secondary-darkest)', marginBottom: '2rem' }}>
          Select a repository to explore its documentation.
        </p>
${cards}
      </main>
    </Layout>
  );
}
`;

fs.mkdirSync(path.dirname(INDEX_OUT), { recursive: true });
fs.writeFileSync(INDEX_OUT, jsx, 'utf-8');
console.log('[cih] wrote', INDEX_OUT);
