# Phase 10c - GitNexus-Style Wiki Intelligence for CIH

## Status

DRAFT

## Summary

Improve `cih-engine wiki` from a mostly deterministic role-based bundle into a richer
documentation generator inspired by GitNexus, while keeping the CIH strengths that
matter for banking codebases:

- deterministic graph facts remain the baseline;
- PO, BA, and Dev pages stay first-class outputs;
- route, DB, topic, process, test, unresolved-reference, and sidecar JSON outputs remain auditable;
- LLM output is grounded in explicit evidence and never replaces the raw graph data.

GitNexus is stronger today at LLM-generated module docs, editable grouping, incremental
regeneration, and a self-contained viewer. CIH should borrow those pieces without
turning the whole wiki into opaque free-form LLM text.

## Current CIH State

Implemented pieces:

- `cih-engine wiki <repo>` reads local analyze and discover JSONL artifacts.
- `crates/cih-wiki` writes a feature-first Docusaurus-ready bundle under `.cih/wiki`.
- Pages include system index, route page, feature index, feature PO, feature BA, and per-community Dev pages.
- `--llm` can enrich each community with short PO, BA, and Dev summaries.
- LLM providers are adapter-based:
  - `openai-compatible`
  - `anthropic`
  - `http-json`
- Evidence packs already include graph facts, snippets, and optional `.md` / `.txt` evidence files.

Important limitations:

- Feature grouping is heuristic and path-based, mainly from `modules/<feature>/`.
- There is no reviewable module tree.
- LLM output is only short summaries, not full module documentation.
- Wiki generation is not commit-aware or incremental.
- There is no self-contained generated HTML viewer.
- Mermaid diagrams are not generated as part of the wiki.
- Provider runtime lacks GitNexus-level hardening such as URL validation, retry-after support,
  streaming progress, saved provider config, and circuit breaking.

## Goals

1. Add an LLM-assisted module tree that can group messy banking code by capability,
   not only by directory shape.
2. Let the user review and edit the module tree before spending tokens on full docs.
3. Generate richer role-based pages from graph evidence and source snippets:
   PO business explanation, BA workflows/contracts, Dev architecture details.
4. Add commit-aware incremental regeneration so unchanged modules are reused.
5. Add an optional self-contained HTML viewer for environments where Docusaurus or yummy
   frontend is not ready.
6. Preserve deterministic graph outputs and manifest metadata so regulated code review remains possible.

## Non-Goals

- Do not replace `cih-wiki` with GitNexus code.
- Do not make LLM mandatory.
- Do not remove the current graph-only wiki mode.
- Do not depend on the `yummy` frontend/backend in this phase.
- Do not require FalkorDB or pgvector for wiki generation.
- Do not publish externally by default.

## Proposed CLI

Extend the existing command:

```bash
cih-engine wiki <repo> [options]
```

New flags:

```bash
--wiki-mode graph|llm-summary|llm-full
--grouping graph|llm|file
--module-tree <path>
--review
--force
--html
--incremental
--max-module-tokens <n>
--save-evidence
```

Rules:

- `graph` mode is the current deterministic mode with no LLM calls.
- `llm-summary` is the current Phase 10b behavior.
- `llm-full` generates richer pages and requires `--llm`.
- `--grouping graph` uses existing feature/community grouping.
- `--grouping llm` asks the LLM to produce a module tree from file list, symbols, routes,
  packages, DB tables, topics, and graph communities.
- `--grouping file` loads a user-edited module tree from `--module-tree`.
- `--review` writes the proposed module tree and exits without generating full LLM pages.
- `--force` ignores cached pages and regenerates everything.
- `--html` writes `.cih/wiki/index.html` as a standalone viewer.
- `--incremental` reuses generated pages when the commit, module membership, graph version,
  prompt version, model, and language are unchanged.
- `--save-evidence` writes evidence packs to `.cih/wiki/evidence/` for debugging and audit.

## Output Bundle

Keep the current `.cih/wiki` root, and add GitNexus-style metadata and viewer files:

```text
.cih/wiki/
  manifest.json
  module_tree.json
  first_module_tree.json
  wiki_meta.json
  index.html                 # optional with --html
  evidence/                  # optional with --save-evidence
    <module-slug>.json
  pages/
    index.md
    routes.md
    routes.json
    <feature>/
      index.md
      po.md
      ba.md
      dev/
        <module-slug>.md
        <module-slug>.json
```

`module_tree.json` is the user-editable tree.
`first_module_tree.json` is the first LLM-generated proposal for reproducibility.
`wiki_meta.json` stores cache keys and incremental generation metadata.

## Module Tree Schema

Add a new serializable type, likely in `cih-wiki`:

```rust
pub struct WikiModuleTree {
    pub schema_version: u32,
    pub generated_at: String,
    pub source: ModuleTreeSource,
    pub repo_commit: Option<String>,
    pub graph_version: String,
    pub community_version: String,
    pub modules: Vec<WikiModuleNode>,
}

pub enum ModuleTreeSource {
    Graph,
    Llm,
    UserEdited,
}

pub struct WikiModuleNode {
    pub id: String,
    pub slug: String,
    pub title: String,
    pub description: Option<String>,
    pub community_ids: Vec<String>,
    pub file_paths: Vec<String>,
    pub children: Vec<WikiModuleNode>,
}
```

Rules:

- IDs and slugs must be deterministic after the tree is accepted.
- A community can belong to one primary module in Phase 10c.
- Files may belong to one primary module in Phase 10c.
- Unknown or ambiguous items fall into `shared` or `uncategorized`.
- The tree must validate before generation:
  - unique IDs;
  - unique slugs;
  - no duplicate file paths inside sibling modules;
  - all referenced communities exist;
  - all referenced files are repo-relative.

## Step 1 - Deterministic Graph Module Tree

Before using LLM grouping, create a graph-derived module tree builder:

- Start from existing `group_communities_by_feature()`.
- Add richer signals:
  - package prefix;
  - Maven/Gradle module;
  - route path segment;
  - DB table prefix;
  - topic/event prefix;
  - class stereotype;
  - process membership.
- Emit `module_tree.json` even in graph-only mode.
- Record the source as `Graph`.

Tests:

- path-based grouping remains stable;
- routes influence feature naming when directory signal is weak;
- DB/topic/process signals are deterministic;
- duplicate slug collision is stable.

## Step 2 - LLM Module Grouping

Add an LLM grouping pass similar to GitNexus but adapted to CIH evidence.

Input evidence for grouping:

- file path;
- package;
- top classes and methods;
- exported or externally referenced symbols;
- routes;
- DB tables;
- topics/events;
- process names;
- community IDs and community names;
- Maven/Gradle module info from `repo-map.json`.

Prompt requirements:

- group by business capability first, technical package second;
- keep modules small enough for later page generation;
- do not invent files or community IDs;
- return strict JSON matching `WikiModuleTree`;
- include short module descriptions;
- use `shared`, `platform`, or `integration` for cross-cutting code when appropriate.

Fallback:

- if the LLM response is invalid, fall back to graph tree and record a warning in manifest.
- if the repo is too large, batch by Maven/Gradle module or top-level path, then merge.

Tests:

- valid grouping JSON parses;
- invalid grouping falls back to graph tree;
- hallucinated file paths are rejected;
- duplicate module slugs are repaired deterministically;
- `--review` writes tree and exits.

## Step 3 - Reviewable Module Tree Workflow

Add:

```bash
cih-engine wiki <repo> --llm --grouping llm --review
```

Behavior:

- Build graph evidence.
- Ask LLM for module tree.
- Write:
  - `.cih/wiki/module_tree.json`
  - `.cih/wiki/first_module_tree.json`
  - `.cih/wiki/manifest.json` with status `review_required`.
- Print instructions:
  - edit `module_tree.json`;
  - rerun `cih-engine wiki <repo> --grouping file --module-tree .cih/wiki/module_tree.json --llm --wiki-mode llm-full`.

Do not generate expensive full pages in review mode.

## Step 4 - Full LLM Page Generation

Add richer LLM output for each accepted module.

The LLM should produce structured markdown sections, not only short summaries.

PO page sections:

- Business purpose
- Main capabilities
- User/business workflows
- APIs involved
- Data touched
- External dependencies
- Open questions from unresolved refs

BA page sections:

- Workflow steps
- API contracts
- Events/topics
- DB tables and data ownership
- Consumes / consumed by
- Business rules found in code or BRD evidence
- Mermaid flow diagram when enough evidence exists

Dev page sections:

- Module responsibility
- Important classes and methods
- Entry points
- Call chains
- Persistence and external integrations
- Tests and coverage signals
- Unresolved references
- Mermaid architecture or flow diagram when enough evidence exists

Implementation rule:

- Keep current deterministic tables below or beside LLM sections.
- Each LLM-generated claim must be grounded by evidence IDs.
- If evidence is weak, the page must say what is known from graph facts and avoid guessing.

Tests:

- renderer keeps deterministic sections when LLM output is absent;
- full LLM sections are inserted in correct page areas;
- unsupported markdown is sanitized;
- evidence citations survive rendering.

## Step 5 - Mermaid Diagram Generation and Sanitization

Add diagram output to BA and Dev pages.

Sources:

- route to method to service to repository flow;
- process steps from `STEP_IN_PROCESS`;
- inter-module calls;
- external topics and HTTP dependencies.

Rules:

- generate Mermaid only when there are at least two connected facts;
- sanitize labels to avoid Mermaid parse failures;
- cap diagram nodes and edges;
- keep a text fallback table when diagram generation is skipped.

Tests:

- special characters in Java names are escaped;
- empty diagrams are not emitted;
- large graphs are capped;
- generated Mermaid passes a lightweight syntax smoke test.

## Step 6 - Incremental Wiki Regeneration

Add `wiki_meta.json`:

```json
{
  "schema_version": 1,
  "repo_commit": "abc123",
  "graph_version": "...",
  "community_version": "...",
  "model": "...",
  "language": "vi",
  "prompt_version": "phase10c-1",
  "module_cache": {
    "order-service": {
      "content_hash": "...",
      "evidence_hash": "...",
      "page_paths": ["pages/order/dev/order-service.md"]
    }
  }
}
```

Cache key should include:

- repo commit if available;
- graph artifact version;
- community artifact version;
- module tree hash;
- module evidence hash;
- model;
- language;
- prompt version.

Behavior:

- if all keys match, reuse existing page;
- if only one module changes, regenerate that module and affected parent/index pages;
- if module tree changes heavily, regenerate all pages;
- `--force` deletes generated wiki content and rebuilds.

Tests:

- unchanged module is skipped;
- changed evidence regenerates only affected module;
- `--force` regenerates all;
- missing git metadata still works using artifact hashes.

## Step 7 - Self-Contained HTML Viewer

Add optional:

```bash
cih-engine wiki <repo> --html
```

Output:

```text
.cih/wiki/index.html
```

Viewer requirements:

- no external CDN for banking/offline environments;
- embedded CSS and JS or local `assets/` files copied into `.cih/wiki/assets`;
- sidebar navigation from `manifest.json`;
- role filters for PO, BA, Dev;
- route page viewer;
- search over page titles and body text;
- Mermaid rendering if a local Mermaid bundle is available, otherwise show source blocks.

This viewer is not the final yummy frontend. It is a local inspection tool like the
GitNexus HTML viewer, but CIH should avoid CDN dependencies.

Tests:

- `index.html` is written;
- embedded manifest/page JSON escapes `</script>`;
- pages can be found by slug;
- no remote `https://cdn...` references exist.

## Step 8 - LLM Runtime Hardening

Improve `crates/cih-engine/src/llm`:

- validate provider URLs:
  - remote endpoints must use HTTPS;
  - HTTP is allowed only for localhost, 127.0.0.1, or ::1;
- support `Retry-After`;
- add exponential backoff with jitter;
- add a simple circuit breaker after repeated failures;
- optionally stream progress for providers that support SSE;
- add saved config under `.cih/llm-config.json` or user config later;
- redact API keys in logs and debug output.

Tests:

- unsafe URL is rejected;
- localhost HTTP is accepted;
- retry-after is honored;
- API key redaction works;
- http-json still supports local no-auth models.

## Step 9 - Manifest Changes

Extend `manifest.json` with:

```json
{
  "generation": {
    "mode": "graph|llm-summary|llm-full",
    "grouping": "graph|llm|file",
    "review_required": false,
    "html_viewer": true,
    "incremental": true
  },
  "module_tree_path": "module_tree.json",
  "wiki_meta_path": "wiki_meta.json",
  "warnings": []
}
```

Do not break old manifest readers:

- keep existing fields stable;
- add optional fields only;
- continue omitting `llm` when not enriched.

## Recommended Implementation Order

1. Add `WikiModuleTree` types, validation, and graph-derived tree output.
2. Add `--grouping`, `--module-tree`, and `--review`.
3. Add LLM grouping prompt and parser with fallback to graph grouping.
4. Add full LLM page content model and insert it into existing PO/BA/Dev renderers.
5. Add Mermaid diagram generation and sanitizer.
6. Add `wiki_meta.json`, cache keys, `--incremental`, and `--force`.
7. Add `--html` standalone viewer.
8. Harden LLM runtime.
9. Update user instructions and Docusaurus docs-viewer notes.

## Acceptance Criteria

- `cargo test -p cih-wiki` passes.
- `cargo test -p cih-engine` passes.
- `cargo test --workspace` passes.
- Graph-only wiki still works without LLM keys.
- `--llm --wiki-mode llm-summary` preserves current Phase 10b behavior.
- `--llm --grouping llm --review` writes a valid editable `module_tree.json` and exits.
- `--grouping file --module-tree <path> --llm --wiki-mode llm-full` generates richer pages.
- `--html` writes a local self-contained viewer.
- Generated pages include role-specific PO, BA, and Dev content.
- Claims generated by the LLM cite evidence IDs or are clearly marked as unknown.
- Large repositories do not require rendering the full graph into one prompt.

## Banking Repo Notes

For the banking repo size already observed:

- 80 custom modules;
- 12,716 custom Java files;
- 26,575 decompiled core library classes;
- 134,548 total methods;
- about 16,000 target business-logic methods;
- Vietnamese BRD documents.

The implementation must avoid single giant prompts. Grouping should use summaries and
module evidence, while full page generation should run module-by-module with concurrency
limits. Decompiled and third-party code should be available as dependency context, but
not treated as primary business modules unless explicitly selected.

