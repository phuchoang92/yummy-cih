use std::path::Path;

use anyhow::{Context, Result};
use serde_json::json;

use crate::manifest::WikiManifest;

pub fn write_html_viewer(out_dir: &Path, manifest: &WikiManifest) -> Result<()> {
    let mut pages = Vec::new();
    for page in &manifest.pages {
        let path = out_dir.join(&page.path);
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read wiki page {}", path.display()))?;
        pages.push(json!({
            "slug": page.slug,
            "title": page.title,
            "role": page.role,
            "kind": page.kind,
            "path": page.path,
            "body": body,
        }));
    }

    let manifest_json = safe_script_json(&serde_json::to_string(manifest)?);
    let pages_json = safe_script_json(&serde_json::to_string(&pages)?);
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>{title} CIH Wiki</title>
<style>
:root {{
  color-scheme: light;
  --bg: #f7f8fb;
  --panel: #ffffff;
  --line: #d9dee8;
  --text: #18202f;
  --muted: #657083;
  --accent: #1266f1;
  --accent-soft: #eaf1ff;
}}
* {{ box-sizing: border-box; }}
body {{
  margin: 0;
  background: var(--bg);
  color: var(--text);
  font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
}}
.shell {{
  display: grid;
  grid-template-columns: 320px minmax(0, 1fr);
  min-height: 100vh;
}}
aside {{
  border-right: 1px solid var(--line);
  background: var(--panel);
  padding: 18px;
  position: sticky;
  top: 0;
  height: 100vh;
  overflow: auto;
}}
main {{
  padding: 28px 40px;
  max-width: 1120px;
  width: 100%;
}}
h1, h2, h3 {{ letter-spacing: 0; }}
.brand {{
  font-size: 18px;
  font-weight: 700;
  margin-bottom: 14px;
}}
.meta {{
  color: var(--muted);
  font-size: 13px;
  line-height: 1.5;
  margin-bottom: 16px;
}}
input, select {{
  width: 100%;
  border: 1px solid var(--line);
  background: #fff;
  border-radius: 6px;
  padding: 10px 11px;
  font-size: 14px;
  margin-bottom: 10px;
}}
.nav-item {{
  border: 0;
  background: transparent;
  width: 100%;
  text-align: left;
  padding: 9px 10px;
  border-radius: 6px;
  cursor: pointer;
  color: var(--text);
  font-size: 14px;
}}
.nav-item:hover, .nav-item.active {{
  background: var(--accent-soft);
  color: var(--accent);
}}
.role {{
  color: var(--muted);
  font-size: 11px;
  text-transform: uppercase;
  letter-spacing: .08em;
  margin-left: 6px;
}}
.content {{
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: 8px;
  padding: 30px;
  box-shadow: 0 8px 30px rgba(24, 32, 47, 0.06);
}}
.content table {{
  border-collapse: collapse;
  width: 100%;
  margin: 16px 0;
}}
.content th, .content td {{
  border: 1px solid var(--line);
  padding: 8px 10px;
  vertical-align: top;
}}
.content th {{ background: #f2f5f9; }}
pre {{
  background: #111827;
  color: #e5e7eb;
  padding: 14px;
  overflow: auto;
  border-radius: 6px;
}}
code {{
  font-family: "SFMono-Regular", Consolas, monospace;
}}
@media (max-width: 860px) {{
  .shell {{ grid-template-columns: 1fr; }}
  aside {{ position: static; height: auto; }}
  main {{ padding: 18px; }}
}}
</style>
</head>
<body>
<div class="shell">
  <aside>
    <div class="brand">CIH Wiki</div>
    <div class="meta" id="meta"></div>
    <input id="search" placeholder="Search pages" />
    <select id="role">
      <option value="">All roles</option>
      <option value="system">System</option>
      <option value="shared">Shared</option>
      <option value="po">PO</option>
      <option value="ba">BA</option>
      <option value="dev">Dev</option>
    </select>
    <div id="nav"></div>
  </aside>
  <main>
    <article class="content" id="content"></article>
  </main>
</div>
<script id="manifest-data" type="application/json">{manifest_json}</script>
<script id="pages-data" type="application/json">{pages_json}</script>
<script>
const manifest = JSON.parse(document.getElementById('manifest-data').textContent);
const pages = JSON.parse(document.getElementById('pages-data').textContent);
const nav = document.getElementById('nav');
const content = document.getElementById('content');
const search = document.getElementById('search');
const role = document.getElementById('role');
let activeSlug = pages[0] ? pages[0].slug : null;
document.getElementById('meta').textContent =
  `${{manifest.repo_name}} · ${{manifest.stats.feature_count || 0}} features · ${{manifest.stats.community_count}} communities`;

function stripFrontmatter(md) {{
  return md.replace(/^---[\s\S]*?---\s*/, '');
}}
function escapeHtml(text) {{
  return text.replace(/[&<>"']/g, ch => ({{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}}[ch]));
}}
function renderMarkdown(md) {{
  const lines = stripFrontmatter(md).split(/\r?\n/);
  let out = [];
  let inCode = false;
  let code = [];
  for (const line of lines) {{
    if (line.startsWith('```')) {{
      if (inCode) {{
        out.push(`<pre><code>${{escapeHtml(code.join('\n'))}}</code></pre>`);
        code = [];
      }}
      inCode = !inCode;
      continue;
    }}
    if (inCode) {{
      code.push(line);
      continue;
    }}
    if (line.startsWith('# ')) out.push(`<h1>${{escapeHtml(line.slice(2))}}</h1>`);
    else if (line.startsWith('## ')) out.push(`<h2>${{escapeHtml(line.slice(3))}}</h2>`);
    else if (line.startsWith('### ')) out.push(`<h3>${{escapeHtml(line.slice(4))}}</h3>`);
    else if (line.startsWith('- ')) out.push(`<li>${{escapeHtml(line.slice(2))}}</li>`);
    else if (line.trim() === '') out.push('');
    else out.push(`<p>${{escapeHtml(line)}}</p>`);
  }}
  return out.join('\n').replace(/(<li>.*<\/li>\n?)+/g, block => `<ul>${{block}}</ul>`);
}}
function filteredPages() {{
  const q = search.value.toLowerCase().trim();
  const r = role.value;
  return pages.filter(p => {{
    const matchesRole = !r || p.role === r || p.kind === r;
    const matchesSearch = !q || p.title.toLowerCase().includes(q) || p.body.toLowerCase().includes(q);
    return matchesRole && matchesSearch;
  }});
}}
function renderNav() {{
  const visible = filteredPages();
  nav.innerHTML = '';
  for (const p of visible) {{
    const button = document.createElement('button');
    button.className = 'nav-item' + (p.slug === activeSlug ? ' active' : '');
    button.innerHTML = `${{escapeHtml(p.title)}} <span class="role">${{escapeHtml(p.kind)}}</span>`;
    button.onclick = () => {{ activeSlug = p.slug; render(); }};
    nav.appendChild(button);
  }}
  if (!visible.find(p => p.slug === activeSlug) && visible[0]) activeSlug = visible[0].slug;
}}
function renderContent() {{
  const page = pages.find(p => p.slug === activeSlug) || pages[0];
  content.innerHTML = page ? renderMarkdown(page.body) : '<h1>No pages</h1>';
}}
function render() {{
  renderNav();
  renderContent();
}}
search.addEventListener('input', render);
role.addEventListener('change', render);
render();
</script>
</body>
</html>
"#,
        title = escape_html_attr(&manifest.repo_name),
        manifest_json = manifest_json,
        pages_json = pages_json,
    );
    std::fs::write(out_dir.join("index.html"), html)?;
    Ok(())
}

fn safe_script_json(json: &str) -> String {
    json.replace("</script", "<\\/script")
}

fn escape_html_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests;

