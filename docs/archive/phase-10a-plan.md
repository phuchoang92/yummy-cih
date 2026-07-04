# Phase 10a - `cih-engine wiki`: Graph Artifacts to Role-Based Wiki Bundle

## Summary

Build the first half of Phase 10 as a file-based content generator.
`cih-engine wiki` reads existing CIH JSONL artifacts from `analyze` and `discover`,
then writes a self-contained wiki bundle for PO, BA, and Dev readers.

Graph-derived content (tables, routes, process chains, class lists) is always produced
and is fully deterministic. Optionally, pass `--llm-enrich` to add a 2–3 sentence
AI-written narrative summary to each community page, making the PO and BA views
genuinely readable rather than formatted data dumps.

Phase 10a does not use FalkorDB, pgvector, or the yummy frontend/backend.
Phase 10b will serve and render the generated bundle inside yummy.

## Inputs

All inputs are local files produced by existing CIH commands.

| File | Produced by | Required | Purpose |
|---|---|---:|---|
| `.cih/artifacts/<v>/nodes.jsonl` | `analyze` | yes | Code graph nodes: classes, methods, routes, fields, endpoints, topics |
| `.cih/artifacts/<v>/edges.jsonl` | `analyze` | yes | Code graph edges: `CALLS`, `HANDLES_ROUTE`, `TESTS`, contracts, dependencies |
| `.cih/artifacts-community/<v>/nodes.jsonl` | `discover` | yes | `Community` and `Process` nodes |
| `.cih/artifacts-community/<v>/edges.jsonl` | `discover` | yes | `MEMBER_OF` and `STEP_IN_PROCESS` edges |
| `.cih/repo-map.json` | `scan` | no | Module list, JAR surface, Spring counts |
| `.cih/artifacts/<v>/unresolved-refs.md` | `analyze` | no | Human-readable unresolved reference report |

If the graph artifacts are missing, the command should return a clear "run analyze first" error.
If the community artifacts are missing, the command should return a clear "run discover first" error.
Optional files must be skipped gracefully.

## Output Bundle

Default output location: `<repo>/.cih/wiki`.

The output location can be overridden:

```bash
cih-engine wiki <repo> --out <dir>
```

Bundle shape:

```text
.cih/wiki/
  manifest.json
  pages/
    shared/
      routes.md
      routes.json
    po/
      index.md
      <community-slug>.md
    ba/
      index.md
      <community-slug>.md
      <community-slug>.json
    dev/
      index.md
      <community-slug>.md
      <community-slug>.json
```

`pages/shared/routes.json` is OpenAPI 3.0.3 only. Do not mix this file with a
custom `RouteInfo` list. If a raw route list is needed later, add a separate
`pages/shared/routes-list.json`.

Every Markdown page includes minimal Docusaurus frontmatter:

```yaml
---
id: <slug>
title: <title>
---
```

The future yummy frontend reader should strip this frontmatter before rendering.
Docusaurus can consume the generated Markdown directly.

## Manifest Schema

Write `manifest.json` with schema version 1:

```json
{
  "schema_version": 1,
  "generated_at": "2026-06-16T10:00:00Z",
  "repo_name": "my-service",
  "graph_version": "<analyze artifact version>",
  "community_version": "<discover artifact version>",
  "stats": {
    "community_count": 12,
    "route_count": 47,
    "process_count": 8,
    "class_count": 312,
    "test_class_count": 41,
    "unresolved_ref_count": 23
  },
  "roles": ["po", "ba", "dev"],
  "nav": {
    "po": [
      { "slug": "po/index", "title": "System Overview", "kind": "index" },
      { "slug": "po/order-service", "title": "order-service", "kind": "community" }
    ],
    "ba": [],
    "dev": []
  },
  "pages": [
    {
      "slug": "po/index",
      "role": "po",
      "title": "System Overview",
      "kind": "index",
      "path": "pages/po/index.md"
    },
    {
      "slug": "ba/order-service",
      "role": "ba",
      "title": "order-service",
      "kind": "community",
      "community_id": "Community:3",
      "path": "pages/ba/order-service.md",
      "json_path": "pages/ba/order-service.json"
    }
  ]
}
```

Field rules:

- `schema_version`: integer, initially `1`.
- `generated_at`: RFC3339 UTC timestamp.
- `graph_version`: copied from the latest analyze `GraphArtifacts.version`.
- `community_version`: copied from the latest discover `GraphArtifacts.version`.
- `roles`: exactly `["po", "ba", "dev"]` for Phase 10a.
- `slug`: URL-safe and unique across all generated pages.
- `kind`: `"index"`, `"community"`, or `"routes"`.
- `json_path`: present only when a JSON sidecar exists.
- `community_id`: present only for community pages.

## Public API

Add a new workspace crate:

```text
crates/cih-wiki/
```

Public interface:

```rust
/// Pre-computed AI summaries for one community (all three roles).
/// Produced by the engine's enrich_communities(); passed into WikiInput.
#[derive(Clone, Debug, Default)]
pub struct CommunityLlmSummary {
    pub po: String,   // 2-3 sentences, business language
    pub ba: String,   // 2-3 sentences, workflow/contract focus
    pub dev: String,  // 2-3 sentences, technical focus
}

pub struct WikiInput<'a> {
    pub nodes: &'a [Node],
    pub edges: &'a [Edge],
    pub community_nodes: &'a [Node],
    pub community_edges: &'a [Edge],
    pub repo_name: String,
    pub graph_version: String,
    pub community_version: String,
    pub unresolved_report: Option<String>,
    pub repo_map: Option<RepoMap>,
    /// Keyed by community_id (e.g. "Community:3"). None = graph-only mode.
    pub llm_summaries: Option<HashMap<String, CommunityLlmSummary>>,
    /// Model name used for enrichment, recorded in manifest. None = not enriched.
    pub llm_model: Option<String>,
}

pub struct WikiOutcome {
    pub out_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub page_count: usize,
    pub community_count: usize,
    pub route_count: usize,
    pub llm_enriched: bool,
}

pub fn generate_wiki(input: WikiInput<'_>, out_dir: &Path) -> Result<WikiOutcome>;
```

`cih-wiki` dependencies:

- `cih-core`
- `serde`
- `serde_json`
- `anyhow`
- `chrono`

Do not depend on `cih-server`. Implement local JSON renderers for OpenAPI and
diagram sidecars inside `cih-wiki`. No HTTP dependency — all LLM calls happen
in `cih-engine/src/wiki_cmd.rs` before `generate_wiki()` is called.

## Engine Command

Add:

```bash
cih-engine wiki <repo> [--out <dir>] [--llm-enrich] [--json]
```

Behavior:

- Read latest analyze artifacts from `<repo>/.cih/artifacts/<v>/`.
- Read latest discover artifacts from `<repo>/.cih/artifacts-community/<v>/`.
- Read optional `<repo>/.cih/repo-map.json`.
- Read optional `<latest-artifacts>/unresolved-refs.md`.
- If `--llm-enrich` (or `ANTHROPIC_API_KEY` is set): call `enrich_communities()` to
  produce LLM summaries before calling `generate_wiki()`.
- Call `cih_wiki::generate_wiki(input, out_dir)`.
- Print a human summary by default.
- Print `WikiOutcome` summary as JSON with `--json`.
- Never connect to FalkorDB or pgvector.

New engine file:

```text
crates/cih-engine/src/wiki_cmd.rs
```

Add a `Wiki` variant to `crates/cih-engine/src/main.rs`.

Clap args for the `Wiki` variant:

```rust
Wiki {
    repo: PathBuf,
    #[arg(long)]
    out: Option<PathBuf>,
    /// Enrich community pages with AI-written summaries via the Anthropic API.
    /// Reads ANTHROPIC_API_KEY from the environment.
    #[arg(long, env = "CIH_LLM_ENRICH")]
    llm_enrich: bool,
    /// Override the default model for LLM enrichment.
    #[arg(long, default_value = "claude-haiku-4-5-20251001")]
    llm_model: String,
    #[arg(long)]
    json: bool,
}
```

## WikiGraph Index

Build a deterministic internal `WikiGraph` once, then pass it to page renderers.
Use `BTreeMap` and sorted vectors for stable output.

Required indexes:

- `nodes_by_id`
- `members_by_community`
- `community_by_member`
- `community_nodes`
- `process_nodes`
- `routes`
- `calls_out`
- `calls_in`
- `tests_out`
- `tests_in`
- `external_calls`
- `publishes`
- `listens`
- `process_steps`
- `community_routes`
- `community_tests`
- `community_class_counts`
- `community_method_counts`
- `community_stereotypes`
- `inter_community_calls`

`process_steps` is required because `STEP_IN_PROCESS` edges are stored as:

```text
symbol -> process
kind = STEP_IN_PROCESS
reason = "step:N"
```

Create:

```rust
pub struct ProcessStep<'a> {
    pub process_id: NodeId,
    pub step_number: usize,
    pub symbol: &'a Node,
}
```

Parse `step_number` from `edge.reason`. If parsing fails, place the step after
numbered steps using a deterministic fallback order by symbol id.

## LLM Enrichment (optional)

### Why

Graph-derived content is correct but dry. Tables of class names and process chains tell a
PO nothing without a sentence that says "this module handles order lifecycle management from
creation through fulfilment." The enrichment layer adds exactly that — one compact paragraph
per community per role — while keeping all factual data graph-grounded.

### Architecture

LLM calls happen **in the engine** (`wiki_cmd.rs`), not inside `cih-wiki`. The wiki crate
stays sync, pure, and testable with no HTTP dependency. Results are passed into `WikiInput`
as a precomputed map.

```rust
// In cih-wiki/src/lib.rs
#[derive(Clone, Debug, Default)]
pub struct CommunityLlmSummary {
    pub po: String,   // 2-3 sentences, business language
    pub ba: String,   // 2-3 sentences, workflow/contract focus
    pub dev: String,  // 2-3 sentences, technical focus
}

// Added to WikiInput
pub llm_summaries: Option<HashMap<String, CommunityLlmSummary>>, // community_id → summary
```

`llm_summaries = None` → graph-only mode (default). All page renderers check this field and
insert the relevant summary at the top of each community page section when present.

### One Anthropic call per community

Use a single structured prompt per community that returns summaries for all three roles.
This keeps cost to N calls (not 3N) and avoids per-role latency stacking.

Prompt template (in `wiki_cmd.rs`):

```
You are writing documentation summaries from a code analysis graph.
Module: "<community-name>"

Graph facts (do not invent anything beyond these):
- Routes: <top 5 route paths, or "none">
- Processes (execution chains): <up to 3, format "A → B → C">
- Class stereotypes: <N controller, N service, N repository, …>
- External dependencies: <ExternalEndpoint and KafkaTopic node names, or "none">
- Calls into: <community names this module calls>
- Called by: <community names that call this module>

Write exactly three JSON fields:
{
  "po": "<2-3 sentences in plain business language — what this module does for users>",
  "ba": "<2-3 sentences on workflows, contracts, and events — what flows in and out>",
  "dev": "<2-3 sentences on technical structure — stereotypes, call patterns, dependencies>"
}
Only output the JSON object. Do not add commentary.
```

### Graceful degradation

- If the API key is absent and `--llm-enrich` is not set: skip silently, no warning.
- If `--llm-enrich` is set but `ANTHROPIC_API_KEY` is missing: exit with a clear error
  message before writing any files.
- If a per-community API call fails (timeout, rate limit, parse error): log a warning,
  store `None` for that community, continue. The rest of the bundle writes normally.
- If the model returns malformed JSON: attempt to extract quoted strings with a simple
  regex fallback; on total failure, skip enrichment for that community.

### HTTP client

Add `ureq` (sync, lightweight, no tokio dependency) to `cih-engine/Cargo.toml`:

```toml
ureq = { version = "2", features = ["json"] }
```

New function in `wiki_cmd.rs`:

```rust
fn enrich_communities(
    community_nodes: &[Node],
    graph: &WikiGraph,
    api_key: &str,
    model: &str,
) -> HashMap<String, CommunityLlmSummary>
```

Calls are sequential (one per community). At ~500 tokens per call with Haiku, a 20-community
repo costs ≈ $0.01 and takes ≈ 10–20 seconds.

### Manifest additions

Add two fields to `manifest.json` when enrichment runs:

```json
"llm_enriched": true,
"llm_model": "claude-haiku-4-5-20251001"
```

Both fields are absent (not `false`) when enrichment is skipped, so old readers that don't
know about them can ignore them without null-handling.

### Where summaries appear in pages

| Page | Section added | Fallback (no LLM) |
|---|---|---|
| `po/<community>.md` | `## Overview\n<po summary>` at top | Section omitted |
| `ba/<community>.md` | `## Workflow Summary\n<ba summary>` at top | Section omitted |
| `dev/<community>.md` | `## Summary\n<dev summary>` at top | Section omitted |
| `po/index.md` | `> AI enrichment active` callout | Nothing |

Index pages are never LLM-generated — only community pages get narrative paragraphs.

### Cost estimate

| Repo size | Communities | Haiku calls | Est. tokens | Est. cost |
|---|---|---|---|---|
| Small | 5 | 5 | 2 500 | < $0.01 |
| Medium | 20 | 20 | 10 000 | ≈ $0.01 |
| Large | 100 | 100 | 50 000 | ≈ $0.05 |

## Slug Policy

Community slug rules:

- Lowercase ASCII.
- Convert any non-alphanumeric separator to `-`.
- Collapse repeated `-`.
- Trim leading/trailing `-`.
- If empty, use `community`.

Collision handling:

- Slugs must be unique within each role and stable across runs.
- If two community names produce the same base slug, suffix the later slug with
  the sanitized community id.
- Example: `order-service-community-3`.

Add tests for collisions such as:

- `Order Service`
- `order-service`
- `Order@Service`

## Page Content

### Shared routes

`pages/shared/routes.md`:

- Title: `API Routes`.
- Table: HTTP method, path, handler, decorator, source file.
- Deterministic sort by path, method, handler id.

`pages/shared/routes.json`:

- OpenAPI 3.0.3 object.
- Include `x-handler-id`, `x-handler-class`, and `x-decorator`.
- Request/response schemas are omitted in Phase 10a.

### PO pages

`pages/po/index.md`:

- System overview.
- Business capabilities table from communities.
- Counts: communities, routes, processes, tests, unresolved references.
- Top entry points from `Process` node props.
- Unresolved external dependencies from `unresolved-refs.md` if present.

`pages/po/<community-slug>.md`:

- Capability summary.
- Routes exposed by the community.
- Process summaries whose entry point is in the community.
- Test summary for symbols in the community.

Tone: plain business language, minimal code detail.

### BA pages

`pages/ba/index.md`:

- Workflow overview.
- Cross-community processes.
- API contracts.
- Event contracts: published and consumed Kafka topics.
- External endpoint usage.

`pages/ba/<community-slug>.md`:

- Ordered workflows using `process_steps`.
- Consumed by: communities with `CALLS` into this community.
- Consumes: communities this community calls.
- Publishes: `PUBLISHES_EVENT` edges from community members.
- Subscribes: `LISTENS_TO` edges from community members.

`pages/ba/<community-slug>.json`:

```json
{
  "format": "community-slice",
  "nodes": [],
  "links": []
}
```

Use this local sidecar shape for Phase 10a. Do not depend on `cih-server` viz helpers.

### Dev pages

`pages/dev/index.md`:

- Technical overview.
- Module summary from `repo-map.json` if available.
- Community summary table.
- Unresolved references section from `unresolved-refs.md` if available.
- JAR dependencies from `repo-map.json` if available.

`pages/dev/<community-slug>.md`:

- Technical reference for one community.
- Class table: class, stereotype, method count, tests.
- Routes handled by the community.
- External calls from community methods.
- Test coverage from `TESTS` edges.
- Important files.

`pages/dev/<community-slug>.json`:

```json
{
  "format": "d3-force",
  "nodes": [],
  "links": []
}
```

Use classes and `CALLS` links within the community. Do not reuse `render_d3_impact()`
from `cih-server`.

## Files to Create or Modify

| File | Change |
|---|---|
| `Cargo.toml` | Add `crates/cih-wiki` to workspace members and workspace dependencies as needed |
| `crates/cih-wiki/Cargo.toml` | New crate manifest (`cih-core`, `serde`, `serde_json`, `anyhow`, `chrono`) |
| `crates/cih-wiki/src/lib.rs` | `CommunityLlmSummary`, `WikiInput`, `WikiOutcome`, `generate_wiki()` |
| `crates/cih-wiki/src/manifest.rs` | Manifest structs and serialization |
| `crates/cih-wiki/src/graph.rs` | `WikiGraph`, indexes, aggregates |
| `crates/cih-wiki/src/slugify.rs` | Slug generation and collision handling |
| `crates/cih-wiki/src/pages/mod.rs` | Page renderer module exports |
| `crates/cih-wiki/src/pages/shared.rs` | Routes Markdown and OpenAPI JSON renderer |
| `crates/cih-wiki/src/pages/po.rs` | PO index and community pages (reads `llm_summaries`) |
| `crates/cih-wiki/src/pages/ba.rs` | BA index, community pages, sidecars (reads `llm_summaries`) |
| `crates/cih-wiki/src/pages/dev.rs` | Dev index, community pages, sidecars (reads `llm_summaries`) |
| `crates/cih-engine/src/wiki_cmd.rs` | `run_wiki()`, summary output, `enrich_communities()` |
| `crates/cih-engine/src/main.rs` | Add `wiki` subcommand wiring with `--llm-enrich` / `--llm-model` |
| `crates/cih-engine/Cargo.toml` | Add `ureq = { version = "2", features = ["json"] }` |

## Implementation Order

1. Add the `cih-wiki` crate to the workspace.
2. Implement `CommunityLlmSummary` + `WikiInput` + `WikiOutcome` structs in `lib.rs`.
3. Implement manifest structs and `manifest_round_trips_json` test.
4. Implement slug generation and collision handling tests.
5. Implement `WikiGraph` indexes and aggregation tests.
6. Implement shared route Markdown and OpenAPI sidecar.
7. Implement PO page renderers (with optional `llm_summaries` section).
8. Implement BA page renderers and sidecars (with optional `llm_summaries` section).
9. Implement Dev page renderers and sidecars (with optional `llm_summaries` section).
10. Implement `generate_wiki()` orchestration and filesystem writes.
11. Add `cih-engine wiki` command wiring (graph-only path).
12. Add `enrich_communities()` in `wiki_cmd.rs` + `ureq` dependency.
13. Wire `--llm-enrich` / `--llm-model` flags; pass `llm_summaries` into `WikiInput`.
14. Add engine tests for artifact loading, missing-discover errors, and `--llm-enrich` skipped.
15. Run package and workspace tests.

## Test Plan

### `cih-wiki` tests (no HTTP, no filesystem — inline fixtures)

- `manifest_round_trips_json`
- `manifest_llm_fields_absent_when_not_enriched` — `llm_enriched`/`llm_model` keys absent in serialized JSON when `None`
- `slugify_converts_community_names`
- `slugify_handles_collisions`
- `wiki_graph_indexes_community_members`
- `wiki_graph_indexes_routes`
- `wiki_graph_orders_process_steps_from_edge_reasons`
- `render_routes_page_produces_table`
- `render_routes_json_is_openapi`
- `render_po_index_lists_communities`
- `render_po_community_shows_routes_and_processes`
- `render_po_community_inserts_llm_summary_when_present` — fixture with `llm_summaries` → Markdown contains `## Overview`
- `render_po_community_omits_overview_section_when_no_summary` — `llm_summaries = None` → no `## Overview` header
- `render_ba_community_shows_inter_community_calls`
- `render_ba_community_writes_sidecar_shape`
- `render_ba_community_inserts_workflow_summary_when_present`
- `render_dev_community_shows_classes`
- `render_dev_community_writes_d3_sidecar_shape`
- `render_dev_community_inserts_technical_summary_when_present`
- `markdown_pages_include_docusaurus_frontmatter`
- `generate_wiki_writes_expected_files`
- `generate_wiki_records_llm_model_in_manifest_when_enriched`

### `cih-engine` tests

- `run_wiki_reads_artifacts_and_writes_default_bundle`
- `run_wiki_writes_custom_out_dir`
- `run_wiki_missing_discover_artifacts_returns_clear_error`
- `run_wiki_skips_missing_optional_repo_map`
- `run_wiki_skips_missing_optional_unresolved_report`
- `enrich_prompt_contains_community_name_and_routes` — unit test for prompt string construction (no HTTP call)
- `enrich_communities_skips_community_on_malformed_response` — inject a malformed JSON response string, assert the community is absent from the result map and no panic

### Commands

Run:

```bash
cargo test -p cih-wiki
cargo test -p cih-engine
cargo test --workspace
```

Manual smoke test on a Java/Spring fixture:

```bash
cargo run -p cih-engine -- scan "$REPO"
cargo run -p cih-engine -- analyze "$REPO" --all --no-load
cargo run -p cih-engine -- discover "$REPO" --no-load

# Graph-only:
cargo run -p cih-engine -- wiki "$REPO"
find "$REPO/.cih/wiki" -maxdepth 3 -type f | sort

# With LLM enrichment:
ANTHROPIC_API_KEY=sk-ant-… cargo run -p cih-engine -- wiki "$REPO" --llm-enrich
cat "$REPO/.cih/wiki/manifest.json" | jq '.llm_enriched, .llm_model'
cat "$REPO/.cih/wiki/pages/po/order-service.md" | head -20
```

## Docusaurus Compatibility

Phase 10a writes Docusaurus-compatible Markdown by default through frontmatter.
No separate `--docusaurus` flag is needed.

The generated `manifest.json` is for yummy and other custom readers. Docusaurus can
ignore it.

## Non-Goals

- `yummy/backend-ts` endpoints: `GET /kb/wiki`, `GET /kb/wiki/page`, `GET /kb/wiki/search`.
- `yummy/frontend` `WikiPanel` redesign.
- BM25 or vector search over wiki pages.
- pgvector enrichment of related links.
- Incremental regeneration (re-run from scratch each time).
- FalkorDB reads.
- LLM-generated index pages (only community pages get narrative summaries).
- Streaming or async LLM calls (sequential blocking calls are sufficient at this scale).

## Assumptions

- Phase 10a only generates the wiki bundle.
- Phase 10b serves this bundle through yummy and redesigns the frontend reader.
- All factual data (tables, routes, process chains) is graph-derived and deterministic.
- LLM summaries are optional narrative layered on top — the bundle is fully usable without them.
- `ANTHROPIC_API_KEY` is provided by the caller; `cih-engine` does not manage credentials.
- pgvector can enrich related links in a later phase, but it is intentionally out of Phase 10a.
