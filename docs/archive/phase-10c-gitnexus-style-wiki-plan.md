# Phase 10c — GitNexus-Style Wiki Intelligence for CIH

## Status

IN PROGRESS

## Summary

Improve `cih-engine wiki` from a mostly deterministic role-based bundle into a richer
documentation generator inspired by GitNexus, while keeping the CIH strengths that
matter for banking codebases:

- deterministic graph facts remain the baseline;
- PO, BA, and Dev pages stay first-class outputs;
- route, DB, topic, process, test, unresolved-reference, and sidecar JSON outputs remain auditable;
- LLM output is grounded in explicit evidence and never replaces the raw graph data.

## What Is Already Implemented

The following are done and must not be reimplemented:

| Item | Location |
|---|---|
| `WikiModuleTree`, `WikiModuleNode` types | `cih-wiki/src/module_tree.rs` |
| `build_graph_module_tree()` | `cih-wiki/src/module_tree.rs` |
| `validate_module_tree()`, `read_module_tree()` | `cih-wiki/src/module_tree.rs` |
| `WikiMeta`, `WikiModuleCacheEntry` schema | `cih-wiki/src/module_tree.rs` |
| `build_wiki_meta()` | `cih-wiki/src/module_tree.rs` |
| `write_html_viewer()` | `cih-wiki/src/html.rs` |
| `WikiManifest.generation`, `.module_tree_path`, `.wiki_meta_path`, `.warnings` | `cih-wiki/src/manifest.rs` |
| `WikiGenerationInfo` with `mode`, `grouping`, `review_required`, `html_viewer`, `incremental` | `cih-wiki/src/manifest.rs` |
| `WikiInput.module_tree`, `.generation`, `.first_module_tree` | `cih-wiki/src/lib.rs` |
| `review_required` early-exit in `generate_wiki()` | `cih-wiki/src/lib.rs` |
| `html_viewer` conditional write in `generate_wiki()` | `cih-wiki/src/lib.rs` |
| `module_tree.json`, `wiki_meta.json` written on every run | `cih-wiki/src/lib.rs` |
| LLM adapter trait, OpenAI, Anthropic, http-json adapters | `cih-engine/src/llm/` |
| Evidence pack builder | `cih-engine/src/llm/evidence.rs` |
| Short LLM summaries (`llm-summary` mode) | `cih-engine/src/wiki_cmd.rs` |

## What Is NOT Yet Implemented (the actual work)

1. CLI flags: `--wiki-mode`, `--grouping`, `--module-tree`, `--review`, `--force`, `--html`,
   `--incremental`, `--max-module-tokens`, `--save-evidence`
2. LLM module grouping pass (`--grouping llm`)
3. Full LLM page content (`--wiki-mode llm-full`) — richer sections beyond short summaries
4. Mermaid diagram generation for BA and Dev pages
5. Incremental cache *read* path — `wiki_meta.json` is written but never read to skip modules
6. LLM runtime hardening — URL validation, retry-after, exponential backoff, circuit breaker,
   API key redaction
7. Evidence file saving to disk (`--save-evidence`)

## Goals

1. Add an LLM-assisted module tree that can group messy banking code by capability,
   not only by directory shape.
2. Let the user review and edit the module tree before spending tokens on full docs.
3. Generate richer role-based pages from graph evidence and source snippets:
   PO business explanation, BA workflows/contracts, Dev architecture details.
4. Add commit-aware incremental regeneration so unchanged modules are reused.
5. Add an optional self-contained HTML viewer for environments where Docusaurus is not ready.
6. Preserve deterministic graph outputs and manifest metadata so regulated code review remains possible.

## Non-Goals

- Do not replace `cih-wiki` with GitNexus code.
- Do not make LLM mandatory.
- Do not remove the current graph-only wiki mode.
- Do not depend on the `yummy` frontend/backend in this phase.
- Do not require FalkorDB or pgvector for wiki generation.
- Do not publish externally by default.

## CLI Flags

Extend the existing command:

```bash
cih-engine wiki <repo> [options]
```

New flags:

```
--wiki-mode <MODE>          graph | llm-summary | llm-full  (default: graph)
--grouping <GROUP>          graph | llm | file               (default: graph)
--module-tree <PATH>        path to user-edited module_tree.json (required with --grouping file)
--review                    write module tree and exit without generating pages
--force                     ignore wiki_meta.json cache, regenerate all pages
--html                      write .cih/wiki/index.html standalone viewer
--incremental               skip modules whose cache key matches wiki_meta.json
--max-module-tokens <N>     evidence token cap per module in llm-full mode (default: 8000 chars)
--save-evidence             write evidence packs to .cih/wiki/evidence/<slug>.json
```

### `--llm` / `--wiki-mode` interaction

| `--llm` | `--wiki-mode` | Effective mode | LLM called |
|---------|--------------|----------------|-----------|
| absent  | absent       | `graph`        | no |
| absent  | `graph`      | `graph`        | no |
| present | absent       | `llm-summary`  | yes (backward compat) |
| absent  | `llm-summary`| `llm-summary`  | yes |
| present | `llm-summary`| `llm-summary`  | yes |
| absent  | `llm-full`   | `llm-full`     | yes |
| present | `llm-full`   | `llm-full`     | yes |

`--llm` with no `--wiki-mode` defaults to `llm-summary` to preserve all existing behavior.
`--wiki-mode llm-full` implies LLM without needing `--llm`.

`--llm-enrich` remains a deprecated hidden alias for `--llm`.

### `--llm-debug-evidence` vs `--save-evidence`

- `--llm-debug-evidence`: prints evidence packs to **stdout** and exits without calling LLM.
  Unchanged. Used for debugging prompts interactively.
- `--save-evidence`: writes each community's evidence pack to
  `.cih/wiki/evidence/<slug>.json` **and** continues to generate pages normally.
  These are not mutually exclusive; both flags can be used together.

## Output Bundle

```text
.cih/wiki/
  manifest.json
  module_tree.json
  first_module_tree.json      (only when --grouping llm --review was run)
  wiki_meta.json
  index.html                  (only with --html)
  evidence/                   (only with --save-evidence)
    <community-slug>.json
  pages/
    index.md
    routes.md
    routes.json
    <feature>/
      index.md
      po.md
      ba.md
      dev/
        <community-slug>.md
        <community-slug>.json
```

`module_tree.json` is user-editable.
`first_module_tree.json` preserves the original LLM proposal before user edits.
`wiki_meta.json` stores cache keys for incremental generation.

## Module Tree Schema

Already implemented in `cih-wiki/src/module_tree.rs`. Schema:

```rust
pub struct WikiModuleTree {
    pub schema_version: u32,
    pub generated_at: String,
    pub source: ModuleTreeSource,       // Graph | Llm | UserEdited
    pub repo_commit: Option<String>,    // from `git rev-parse HEAD`, None if not a git repo
    pub graph_version: String,
    pub community_version: String,
    pub modules: Vec<WikiModuleNode>,
}

pub struct WikiModuleNode {
    pub id: String,
    pub slug: String,
    pub title: String,
    pub description: Option<String>,
    pub community_ids: Vec<String>,     // communities at this level
    pub file_paths: Vec<String>,        // repo-relative
    pub children: Vec<WikiModuleNode>,  // sub-modules (communities as children of features)
}
```

`children` in Phase 10c: the graph builder populates features as top-level nodes with their
communities as children. When a user edits the tree, they may restructure children freely.
The wiki renderer uses the flat `community_ids` list at each level for page generation;
children are for navigation and future drill-down only.

Validation rules (already implemented):
- unique IDs and slugs;
- no duplicate file paths within sibling modules;
- all referenced community IDs must exist in the graph;
- all file paths must be repo-relative (no `..`, no absolute paths).

`repo_commit` source: run `git -C <repo> rev-parse HEAD` as a subprocess; if the command
fails (not a git repo, no commits), set `None`. Never fail the wiki run if git is unavailable.

## Step 1 — Deterministic Graph Module Tree

**Status: DONE.** See `cih-wiki/src/module_tree.rs`.

The graph-derived tree is built from `group_communities_by_feature()` extended with:
- Maven/Gradle module info from `repo-map.json`;
- member file paths for navigation;
- community names as child nodes.

Written to `module_tree.json` on every wiki run, regardless of mode.

## Step 2 — LLM Module Grouping

Add a grouping pass: `--grouping llm`.

### Evidence for grouping

For each community, collect a compact summary (not the full evidence pack):
- community name and ID;
- top 5 file paths;
- exported routes (method + path);
- DB table names;
- topic/event names;
- Maven module name if known;
- class stereotype counts.

### Token budget and batching

The banking repo has ~80 communities. A single prompt listing all community summaries is
approximately 12,000–20,000 characters — within a 128k-context model but large.

Batching strategy:
- If total evidence ≤ 20,000 chars: one prompt for all communities.
- If total evidence > 20,000 chars: batch by Maven/Gradle module (from `repo_map.json`).
  - Each batch covers one Maven module's communities.
  - A merge pass asks the LLM to rationalize duplicate feature names across batches.
  - If no `repo_map.json`: batch by top-level path prefix, max 30 communities per batch.
- Record `batch_count` in the manifest warnings if batching was used.

### Prompt requirements

- Group by business capability first, technical package second.
- Keep modules small enough for later page generation (prefer 2–8 communities per module).
- Do not invent community IDs or names; only reference the IDs provided.
- Return strict JSON matching a flat `WikiModuleLlmResponse` (see below).
- Use `shared`, `platform`, or `integration` for cross-cutting code.

### Response schema

The LLM returns only the grouping decisions, not the full `WikiModuleTree` metadata:

```json
{
  "modules": [
    {
      "slug": "payment",
      "title": "Payment",
      "description": "Handles payment processing and refunds.",
      "community_ids": ["Community:3", "Community:7"]
    }
  ]
}
```

The engine fills in `id`, `file_paths`, `children`, `schema_version`, `generated_at`,
`repo_commit`, `graph_version`, `community_version` after parsing the response.

### Validation and fallback

- If the LLM response JSON is invalid: fall back to graph tree, add warning to manifest.
- Reject any `community_id` not present in the graph; log and drop the reference.
- Repair duplicate slugs by appending `-2`, `-3`, etc.
- Communities not assigned to any module: placed in a synthetic `shared` module.
- Fallback is always the graph-derived tree, never an empty tree.

## Step 3 — Reviewable Module Tree Workflow

Add `--review` flag behavior:

```bash
cih-engine wiki <repo> --grouping llm --review
```

Behavior:
1. Build graph evidence.
2. Call LLM grouping (one or more batched calls).
3. Write:
   - `.cih/wiki/module_tree.json` — the proposed grouping (edit this)
   - `.cih/wiki/first_module_tree.json` — the original proposal (for reference)
   - `.cih/wiki/manifest.json` with `generation.review_required: true`
4. Print instructions:
   ```
   Module tree written to: .cih/wiki/module_tree.json
   Review and edit, then re-run:
     cih-engine wiki <repo> --grouping file --module-tree .cih/wiki/module_tree.json --wiki-mode llm-full
   ```
5. Exit with code 0.

`--review` without `--grouping llm` uses the graph-derived tree (useful to inspect grouping
before committing to LLM calls).

Already partially wired in `generate_wiki()` via `generation.review_required`. Needs CLI
flags and the grouping call wired in `wiki_cmd.rs`.

## Step 4 — Full LLM Page Generation (`--wiki-mode llm-full`)

Add richer LLM output per community. This is a **new content model** alongside the existing
short summary.

### New type: `CommunityLlmFull`

```rust
pub struct CommunityLlmFull {
    // PO sections
    pub po_summary: String,
    pub po_capabilities: String,    // bullet points
    pub po_workflows: String,
    pub po_open_questions: String,
    // BA sections
    pub ba_process_overview: String,
    pub ba_contracts: String,
    pub ba_business_rules: String,
    // Dev sections
    pub dev_responsibility: String,
    pub dev_key_classes: String,
    pub dev_entry_points: String,
}
```

`CommunityLlmSummary` (`po`, `ba`, `dev` strings) is unchanged and used in `llm-summary` mode.

### Evidence budget for `llm-full`

Use the standard evidence pack (routes, stereotypes, dependencies, tables, events, snippets,
BRD chunks) with a raised cap: **8,000 characters** instead of 3,000.
Truncation order is the same: BRD → snippets → callers/callees → routes.
`--max-module-tokens` overrides the cap (default 8000).

### LLM prompt and response format

The LLM returns JSON with nested objects:

```json
{
  "po": {
    "summary": "2-3 sentence business overview. Cite evidence IDs.",
    "capabilities": "- Can do X [R1]\n- Manages Y [T1]",
    "workflows": "Step 1: ... Step 2: ...",
    "open_questions": "What is the SLA for payment retries?"
  },
  "ba": {
    "process_overview": "...",
    "contracts": "Provides GET /orders [R1]. Consumes payment events [E1].",
    "business_rules": "Orders over 10M VND require dual approval [B1]."
  },
  "dev": {
    "responsibility": "Owns order lifecycle from creation to fulfillment.",
    "key_classes": "- OrderService: orchestrates [S1]\n- OrderRepository: persistence [T1]",
    "entry_points": "POST /orders → OrderController.create [R1]"
  }
}
```

### Page integration

Each page renderer gains an optional `full: Option<&CommunityLlmFull>` parameter.
When present, full-mode sections are inserted **above** the deterministic tables.
Deterministic tables (routes, DB access, classes) are always rendered.
When `full` is absent, the page looks identical to the current graph-only or summary output.

### Evidence citation requirement

The system prompt must tell the LLM: "every claim must cite at least one evidence ID
(R1, T1, S1, B1, etc.) from the evidence pack, or say 'unknown from evidence'."

## Step 5 — Mermaid Diagram Generation

Add deterministic Mermaid diagrams to BA and Dev pages. These are graph-derived, not LLM-generated.

### Sources

- **BA page**: process steps from `STEP_IN_PROCESS` edges for the feature's communities.
- **Dev page**: inter-community call graph involving this community.

### Rules

- Generate `flowchart LR` diagrams only.
- Emit only when there are at least 2 connected nodes.
- Cap at 20 nodes and 30 edges; if exceeded, truncate and note "diagram truncated".
- Sanitize labels: replace `"`, `<`, `>`, `[`, `]`, `(`, `)`, `--` with safe alternatives.
  Use `string["label"]` node syntax to allow spaces and special characters safely.
- Text fallback: if diagram is skipped, emit the same information as a markdown table.

### Smoke test (label-escaping only)

The Rust test checks:
- Generated Mermaid starts with `flowchart LR`.
- No raw `"` characters appear inside node labels (use escaped form).
- No `--` sequences inside labels (would be parsed as arrow).
- Output length does not exceed a reasonable cap.

No external syntax validator is used.

### Mermaid bundle for `--html`

The standalone HTML viewer (`html.rs`) already renders page markdown as plain HTML.
Mermaid diagrams in the markdown appear as fenced code blocks with `mermaid` language tag.
The viewer renders them as `<pre><code class="language-mermaid">...</code></pre>`.
No Mermaid JS bundle is embedded in this phase; the code block is readable as text.
A future enhancement can add the Mermaid JS bundle.

## Step 6 — Incremental Wiki Regeneration

`wiki_meta.json` is already written on every run. This step adds the **read** path.

### Cache key per community

```
SHA256(community_id + "|" + evidence_text + "|" + model + "|" + language + "|" + prompt_version)
```

Where `evidence_text` is the rendered evidence pack string.
Use SHA-256 via the `sha2` crate already in the workspace, or a lightweight Fnv/FxHash.
Store as a 16-character hex string of the first 8 bytes.

### Behavior with `--incremental`

1. At wiki run start, read `.cih/wiki/wiki_meta.json` if it exists.
2. For each community with an LLM call: compute its cache key.
3. If the key matches `module_cache[community_id].evidence_hash` and the page files in
   `module_cache[community_id].page_paths` all exist on disk: skip the LLM call, reuse pages.
4. After all communities: write updated `wiki_meta.json` with new/changed keys.
5. `--force` deletes the in-memory cache before the run, forcing regeneration of all pages.

### `repo_commit` in the cache key

If `git -C <repo> rev-parse HEAD` succeeds, its output is stored in `WikiMeta.repo_commit`.
The cache key does NOT include `repo_commit` directly — the `graph_version` and
`community_version` already change when the graph changes. `repo_commit` is metadata only.

### Missing git

If git is unavailable, `repo_commit` is `None`. Incremental still works using artifact hashes.

## Step 7 — Self-Contained HTML Viewer

**Status: DONE.** `cih-wiki/src/html.rs` already implements the full viewer.

The viewer is written when `generation.html_viewer` is `true`, which is set when `--html`
is passed. The CLI flag needs to be wired in `main.rs`/`wiki_cmd.rs` (Step 1 of the
remaining work).

The viewer includes:
- embedded CSS and JS (no CDN);
- sidebar navigation from `manifest.json`;
- role filter for PO, BA, Dev, System, Shared;
- full-text search over page titles and bodies;
- lightweight markdown renderer.

No Mermaid JS is embedded in this phase.

## Step 8 — LLM Runtime Hardening

Improve `crates/cih-engine/src/llm/`:

### URL validation

Validate the base URL before making any HTTP call:
- Parse the URL; reject malformed URLs.
- Remote endpoints (not localhost/127.0.0.1/::1) **must** use HTTPS.
- HTTP is allowed for `localhost`, `127.0.0.1`, `[::1]`, and `0.0.0.0`.
- `http-json` URLs from config files are also validated at load time.

### Retry-After support

When an adapter receives HTTP 429 (Too Many Requests):
- Check the `Retry-After` response header (seconds or HTTP-date).
- Sleep for `min(retry_after, 60)` seconds before the next attempt.
- Count against the existing retry budget.

### Exponential backoff with jitter

Replace the current linear `500ms * attempt` sleep with:
```
delay = min(base_ms * 2^attempt, max_ms) + jitter
jitter = (attempt * 137) % (base_ms / 2)   // deterministic, avoids thread-rng dep
base_ms = 500, max_ms = 30_000
```

### Circuit breaker

After `circuit_break_threshold` (default: 5) consecutive failures across all communities
in a single wiki run, skip all remaining LLM calls and record a `CIRCUIT_OPEN` warning
in the manifest. Communities that were skipped appear in `failed_community_ids`.

### API key redaction

Any error message that might echo the request URL or headers must have the API key value
replaced with `***`. Implementation: after constructing the error string, search for the
key value and replace it.

### SSE / streaming (explicitly out of scope for Phase 10c)

Streaming requires replacing `ureq` (synchronous) with an async HTTP client (`reqwest` +
`tokio`), which is a significant dependency change. Defer to a later phase.

### Saved LLM config

`--save-provider-config` and a persistent `~/.cih/llm-config.json` are out of scope
for Phase 10c. The user continues to pass flags or environment variables.

## Step 9 — Manifest Changes

**Status: DONE** for the schema. The manifest already has `generation`, `module_tree_path`,
`wiki_meta_path`, `warnings`. These are written correctly for all modes.

The only remaining work is ensuring `generation.mode` and `generation.grouping` are set
from the new CLI flags when they are wired.

## Recommended Implementation Order

Steps 7 and 9 are already done. Proceed in this order:

1. **Wire CLI flags** (`main.rs` + `wiki_cmd.rs`) — unlocks all features, no new logic needed.
2. **Step 5** — Mermaid diagram generator (`cih-wiki/src/mermaid.rs`) — no external deps.
3. **Step 4** — Full LLM content model + page renderer extensions + full-page prompt.
4. **Step 2** — LLM module grouping (`cih-engine/src/llm/grouping.rs`) + review workflow.
5. **Step 6** — Incremental cache read path (`wiki_cmd.rs` + `module_tree.rs`).
6. **Step 8** — Runtime hardening (URL validation, retry-after, backoff, circuit breaker, redaction).

## Acceptance Criteria

- `cargo test -p cih-wiki` passes.
- `cargo test -p cih-engine` passes.
- `cargo test --workspace` passes.
- Graph-only wiki still works without LLM keys.
- `--wiki-mode llm-summary` (or `--llm`) preserves current Phase 10b behavior.
- `--wiki-mode llm-full` generates richer pages with structured sections.
- `--grouping llm --review` writes a valid editable `module_tree.json` and exits.
- `--grouping file --module-tree <path>` uses the user-edited tree.
- `--html` writes a local self-contained viewer with no CDN refs.
- `--incremental` skips communities whose evidence hash is unchanged.
- `--save-evidence` writes per-community evidence packs to `.cih/wiki/evidence/`.
- Remote LLM URLs without HTTPS are rejected with a clear error.
- API keys do not appear in error messages.
- Large repos (80+ communities) complete without OOM or single giant prompts.

## Banking Repo Notes

- 80 custom modules;
- 12,716 custom Java files;
- 26,575 decompiled core library classes;
- 134,548 total methods;
- about 16,000 target business-logic methods;
- Vietnamese BRD documents.

For LLM grouping: batch communities by Maven module (typically 5–15 per module).
For `llm-full`: each community generates one LLM call with up to 8,000-char evidence.
Decompiled code is excluded from LLM evidence (no `CommunityLlmFull` for decompiled communities).
Concurrency: 8 parallel LLM calls by default; `--llm-concurrency` overrides.
