# Phase 10b — LLM-First Role Wiki Implementation Plan

## Goal

Upgrade `cih-engine wiki` from graph-table docs into an evidence-grounded documentation
generator with a navigable feature-first page hierarchy. The LLM writes narrative sections;
CIH owns the facts. Graph-only mode remains the default; `--llm` opts in to network calls.

---

## Page Hierarchy (Feature-First)

Top level is the **feature/module** derived from the Java package path, not the audience role.
PO and BA views are aggregated at feature level. Dev is per-community within the feature.

```
pages/
  index.md                          # system overview: feature list, total routes/tables
  open-questions.md                 # aggregated open questions (--llm only)
  <feature>/                        # e.g. payment/, order/, pos/, product/
    index.md                        # feature landing: links to po/ba/dev, key stats
    po.md                           # business view: routes, tables, LLM narrative
    ba.md                           # workflow view: process chains, dependencies
    dev/
      <primary-class-slug>.md       # per-community technical reference
      <primary-class-slug>-2.md     # suffix only on slug collision within the feature
  shared/                           # cross-cutting communities (no modules/ match)
    index.md
    po.md
    ba.md
    dev/
      <primary-class-slug>.md
  routes.md                         # global route list (existing, unchanged)
  openapi.json                      # OpenAPI spec (existing, unchanged)
```

**Current repo stats (212ecom-be):**
18 features (product 71 communities, auth 47, payment 18, …), 22 cross-cutting → `shared/`.
Total pages: 18 × 3 feature pages + 351 dev pages + top-level + shared ≈ 430 pages
(vs 1 057 flat pages today — fewer pages, but each is meaningful and navigable).

---

## Decisions Made (Pre-Implementation)

### Feature inference

Scan member nodes' `file` paths for `modules/<feature>/`. Take the most frequent segment
across all community members. Fall back to `shared` when no `modules/` match is found (covers
cross-cutting utilities, constants, and base classes).

Algorithm:
```
for each method_node in community.members:
    if file matches r"modules/([^/]+)/":
        count[capture] += 1
feature = count.most_common()[0] if count else "shared"
```

This is computed once in `wiki_evidence.rs` and stored as `WikiEvidencePack.feature`.

### Dev page slug (primary class name)

1. Collect all class nodes in the community (inferred from method IDs: strip method suffix,
   replace `Method:` with `Class:`).
2. Filter out test classes (file path contains `/test/`).
3. Among remaining, prefer the class whose simple name most closely matches the feature name
   (e.g., `PaymentController` for feature `payment`). Tiebreak: alphabetical.
4. Convert the chosen class name from PascalCase to kebab-case:
   `PaymentOrchestrationService` → `payment-orchestration-service`.
5. If the slug already exists in the feature folder (collision), append `-2`, `-3`, etc.
6. If all classes are test classes, use the test class name with suffix `-test`.

### Feature-level pages vs per-community pages

| Page type | Scope | Audience |
|---|---|---|
| `<feature>/po.md` | All communities in the feature | PO — business rules, routes, tables |
| `<feature>/ba.md` | All communities in the feature | BA — workflows, dependencies, data access |
| `<feature>/dev/<class>.md` | Single community | Dev — class/method detail, signatures, calls |
| `<feature>/index.md` | All communities in the feature | All — landing page with stats and links |

PO and BA no longer have per-community pages. Developers navigate to `dev/<class>.md` for
detail, then read `po.md` or `ba.md` for the business context of the surrounding feature.

### LLM call targets

- **Per-feature call** → writes the narrative sections of `<feature>/po.md` and `<feature>/ba.md`.
  Evidence pack aggregates top-5 communities (by route count) from the feature.
  Token budget: 10 000 input, 3 000 output per feature call.
- **Per-community call** → writes the narrative section of `<feature>/dev/<class>.md`.
  Evidence pack is the single community.
  Token budget: 6 000 input, 2 000 output per community call.

Total LLM calls for 212ecom-be: 18 feature + 351 community = 369 calls ≈ 3.3 M tokens (~$4–10).

### `--llm-enrich` deprecation — Option A

Becomes an alias for `--llm` with a stripped-down evidence pack (no source snippets, no BRD).
Old Anthropic-specific HTTP code removed. One HTTP client. Deprecation warning to stderr.

### Token budget (per community — dev pages)

| Slot | Limit |
|---|---|
| Input total | 6 000 tokens |
| Output total | 2 000 tokens |
| Source snippets | ≤ 1 500 (truncated first on overflow) |
| BRD chunks | ≤ 1 500 (truncated second) |
| Graph facts | remainder, routes never dropped |

Token budget (per feature — po/ba pages):

| Slot | Limit |
|---|---|
| Input total | 10 000 tokens (top-5 communities aggregated) |
| Output total | 3 000 tokens |

### LLM parallelism

Up to **8 concurrent calls** (configurable via `--llm-concurrency`). Feature calls and
community calls share the same pool. Use `rayon` thread pool — no `tokio` added.

### Citation ID format

```
<page-path>/<source>/<index>
```

Examples:
- `payment/payment-controller/graph/route-0`
- `payment/payment-controller/snippet/PaymentController.java:61`
- `payment/payment-controller/brd/requirements.md:chunk-3`

Citation IDs use the final page path (feature + dev slug) so they remain stable and unique
across the whole wiki output. `[cite:<id>]` inline in LLM text → rendered as footnote.

### `open_questions`

Rendered under `## Open Questions` on each `<feature>/po.md` where present.
Also aggregated into top-level `pages/open-questions.md`.

### BRD chunk ranking

Reuses `cih-search` BM25. Build query from routes, class names, method names, DB tables.
Do not re-implement ranking in `cih-engine`.

### Source snippets

`wiki --llm` requires repo source files to be present. Clear error if absent.

### `.docx`

Deferred. `.md` and `.txt` only. `.docx` is a `--feature docx` stub that warns and skips.

### `--json` vs `--llm-debug-evidence`

`--json` only changes stat output format. Evidence pack dumps require `--llm-debug-evidence`.

---

## Files to Create / Modify

| File | Action | Description |
|---|---|---|
| `crates/cih-engine/src/wiki_cmd.rs` | Modify | CLI + orchestration only after split |
| `crates/cih-engine/src/wiki_evidence.rs` | **Create** | `WikiEvidencePack`, feature inference, slug generation |
| `crates/cih-engine/src/wiki_llm.rs` | **Create** | HTTP client, retry, JSON parsing |
| `crates/cih-engine/src/wiki_brd.rs` | **Create** | BRD loading, chunking, BM25 ranking |
| `crates/cih-wiki/src/lib.rs` | Modify | Add `CommunityLlmDoc`, `FeatureLlmDoc`, `WikiInput` fields |
| `crates/cih-wiki/src/pages/feature_index.rs` | **Create** | Feature landing page renderer |
| `crates/cih-wiki/src/pages/feature_po.rs` | **Create** | Feature PO page renderer (aggregated) |
| `crates/cih-wiki/src/pages/feature_ba.rs` | **Create** | Feature BA page renderer (aggregated) |
| `crates/cih-wiki/src/pages/dev.rs` | Modify | Add `CommunityLlmDoc` narrative sections |
| `crates/cih-wiki/src/pages/open_questions.rs` | **Create** | Top-level open-questions aggregation |
| `crates/cih-wiki/src/pages/system_index.rs` | **Create** | `pages/index.md` system overview |
| `crates/cih-wiki/src/manifest.rs` | Modify | New fields; update page paths |
| `crates/cih-wiki/src/generate.rs` | Modify | Drive new hierarchy; write feature + dev pages |

The existing `po.rs` and `ba.rs` become internal helpers consumed by `feature_po.rs` /
`feature_ba.rs` for per-community table rendering — they are not deleted but their
`render_*_community()` functions are called from the aggregated feature renderers.

---

## Step 1 — Split `wiki_cmd.rs`

Move code without behaviour changes. All existing tests must pass after this step.

**`wiki_cmd.rs` keeps:** `run_wiki()`, `latest_community_artifacts()`.

**Move to `wiki_evidence.rs`:** `WikiEvidencePack` stub, feature inference, slug generation.

**Move to `wiki_llm.rs`:** `enrich_one_community()` renamed to `LlmClient::call()`.

**Move to `wiki_brd.rs`:** `EvidenceChunk` stub, `load_evidence_files()` stub.

---

## Step 2 — New CLI Flags

```
--llm                         Full LLM generation (requires API key)
--llm-enrich                  [Deprecated] Alias for --llm with no snippets/BRD
--llm-base-url <URL>          OpenAI-compatible base URL  [env: CIH_LLM_BASE_URL]
--llm-model <MODEL>           [env: CIH_LLM_MODEL, default: gpt-4o-mini]
--llm-timeout-secs <N>        Per-request timeout [default: 60]
--llm-retries <N>             [default: 3]
--llm-concurrency <N>         [default: 8]
--llm-debug-evidence          Write evidence packs to <out>/evidence/<page-path>.json
--llm-dry-run                 Build evidence packs, print token estimates, no LLM calls
--evidence <path-or-glob>     BRD/requirements files; repeatable
--brd <path-or-glob>          Alias for --evidence
```

API key order: `CIH_LLM_API_KEY` → `OPENAI_API_KEY`. Remove `ANTHROPIC_API_KEY` lookup.

---

## Step 3 — Feature Groups (`wiki_evidence.rs`)

```rust
pub struct FeatureGroup {
    pub feature: String,               // "payment", "shared", etc.
    pub community_ids: Vec<String>,
}

pub fn group_communities_by_feature(
    communities: &[Node],
    graph: &WikiGraph,
) -> Vec<FeatureGroup>
```

The grouper runs before any page rendering. Its output drives which communities go under
which feature folder and feeds into all subsequent steps.

---

## Step 4 — Slug Generation (`wiki_evidence.rs`)

```rust
pub fn primary_class_slug(community_id: &str, graph: &WikiGraph, feature: &str) -> String
pub fn dedup_slugs(groups: &mut [FeatureGroup], graph: &WikiGraph)
```

Logic: get non-test classes → prefer class name containing the feature → kebab-case →
collision → append `-2`. `dedup_slugs` runs across all communities in a feature at once.

---

## Step 5 — `WikiEvidencePack` (`wiki_evidence.rs`)

```rust
pub struct WikiEvidencePack {
    pub community_id: String,
    pub feature: String,
    pub page_path: String,             // e.g. "payment/dev/payment-controller"
    pub routes: Vec<RouteEvidence>,
    pub process_steps: Vec<ProcessEvidence>,
    pub classes: Vec<ClassEvidence>,
    pub methods: Vec<MethodEvidence>,
    pub db_tables: Vec<DbTableEvidence>,
    pub kafka_topics: Vec<TopicEvidence>,
    pub external_calls: Vec<String>,
    pub tests: Vec<TestEvidence>,
    pub source_snippets: Vec<SourceSnippet>,
    pub brd_chunks: Vec<BrdChunk>,
    pub unresolved_refs: Vec<String>,
    pub token_estimate: usize,
}

pub struct FeatureEvidencePack {
    pub feature: String,
    pub top_communities: Vec<WikiEvidencePack>,  // top-5 by route count
    pub all_routes: Vec<RouteEvidence>,
    pub all_db_tables: Vec<DbTableEvidence>,
    pub all_process_steps: Vec<ProcessEvidence>,
    pub brd_chunks: Vec<BrdChunk>,
    pub token_estimate: usize,
}
```

Builder: populate from `WikiGraph`, read source snippets from repo path, trim to budget,
assign citation IDs using the `page_path` prefix.

---

## Step 6 — BRD Loading (`wiki_brd.rs`)

```rust
pub struct EvidenceChunk { pub source_file: String, pub chunk_index: usize, pub text: String }

pub fn load_evidence_files(paths: &[PathBuf]) -> Vec<EvidenceChunk>
pub fn rank_chunks_for_community(chunks: &[EvidenceChunk], pack: &WikiEvidencePack, top_k: usize) -> Vec<(EvidenceChunk, f32)>
pub fn rank_chunks_for_feature(chunks: &[EvidenceChunk], pack: &FeatureEvidencePack, top_k: usize) -> Vec<(EvidenceChunk, f32)>
```

Chunking: ~400 tokens (1 600 chars) with 50-token overlap.
Ranking: BM25 from `cih-search`. `.docx` → feature-gated stub that warns and returns empty.

---

## Step 7 — LLM Client (`wiki_llm.rs`)

```rust
pub struct LlmClient { base_url, api_key, model, timeout_secs, retries }

impl LlmClient {
    pub fn call_community(&self, pack: &WikiEvidencePack) -> Result<CommunityLlmDoc>
    pub fn call_feature(&self, pack: &FeatureEvidencePack) -> Result<FeatureLlmDoc>
}
```

Use `ureq` (already a dependency). Retry on 429/5xx: `2^attempt` seconds, up to `retries`
times. On non-JSON response: one retry appending "respond with valid JSON only". After
exhausted retries: `Err(...)`, caller writes graph-only fallback and records failure.

**Community prompt schema:**
```json
{
  "po": { "purpose": "...", "capabilities": [...], "business_rules": [...],
          "user_impact": "...", "risks": [...], "open_questions": [...] },
  "ba": { "workflow": "...", "inputs": [...], "outputs": [...],
          "validations": [...], "exceptions": [...],
          "upstream": [...], "downstream": [...] },
  "dev": { "entrypoints": [...], "structure": "...", "data_access": "...",
           "integrations": [...], "unresolved": [...], "tests": "..." }
}
```

**Feature prompt schema:**
```json
{
  "po": { "purpose": "...", "capabilities": [...], "business_rules": [...],
          "user_impact": "...", "risks": [...], "open_questions": [...] },
  "ba": { "workflow": "...", "process_overview": "...", "upstream": [...], "downstream": [...] }
}
```

---

## Step 8 — Data Types (`cih-wiki/src/lib.rs`)

```rust
pub struct CommunityLlmDoc { pub po: PoDoc, pub ba: BaDoc, pub dev: DevDoc }
pub struct FeatureLlmDoc   { pub po: PoDoc, pub ba: BaFeatureDoc }

// PoDoc, BaDoc, DevDoc — structured fields as before
// BaFeatureDoc: { workflow, process_overview, upstream, downstream }
```

`WikiInput` gains:
```rust
pub feature_groups: Vec<FeatureGroup>,          // computed by wiki_evidence.rs
pub community_slugs: HashMap<String, String>,   // community_id → primary-class-slug
pub community_features: HashMap<String, String>,// community_id → feature
pub llm_docs: Option<HashMap<String, CommunityLlmDoc>>,
pub feature_llm_docs: Option<HashMap<String, FeatureLlmDoc>>,
```

`CommunityLlmSummary` and `llm_summaries` kept but marked `#[deprecated]`.

---

## Step 9 — Renderers (`cih-wiki/src/pages/`)

### Feature index (`feature_index.rs`)
```markdown
---
id: <feature>/index
title: <Feature> Module
---
# <Feature> Module

<stat line: N communities · M routes · K tables>

→ [Business View](po.md) · [Workflows](ba.md)

## Components
| Class | Role | File |
|---|---|---|
| PaymentController | controller | PaymentController.java |
...

## Routes
<aggregated route table>
```

### Feature PO (`feature_po.rs`)
```markdown
---
id: <feature>/po
title: <Feature> — Business View
---
## Purpose          ← LLM (or omitted in graph-only)
## Capabilities     ← LLM or empty
## Business Rules   ← LLM or empty
## User Impact      ← LLM or empty
## Risks            ← LLM or empty
## Open Questions   ← LLM or empty
---
## Routes           ← deterministic (all routes in feature)
## Core Tables      ← deterministic (all tables in feature)
## Sources          ← citations (--llm only)
```

### Feature BA (`feature_ba.rs`)
```markdown
---
id: <feature>/ba
title: <Feature> — Workflows
---
## Workflow Overview   ← LLM or empty
## Process Overview    ← LLM or empty
## Upstream / Downstream ← LLM or deterministic inter-community calls
---
## Workflows           ← deterministic process steps with file:line
## Data Access         ← deterministic DB tables
## API Surface         ← deterministic routes
## Sources             ← citations (--llm only)
```

### Dev community (`dev.rs` — existing, enhanced)
```markdown
---
id: <feature>/dev/<primary-class-slug>
---
## Overview        ← LLM (or omitted)
## Entrypoints     ← LLM or empty
---
## Classes         ← deterministic (file:line, signatures, calls) ← already implemented
## Routes          ← deterministic
## DB Access       ← deterministic
## Sources         ← citations
```

### System index (`system_index.rs`)
```markdown
# System Overview
<N features · M routes · K communities>
## Features
| Feature | Communities | Routes | Tables |
|---|---|---|---|
| payment | 18 | 12 | 5 |
...
```

### Open questions (`open_questions.rs`)
```markdown
# Open Questions
| Feature | Question |
|---|---|
| payment | Does this module handle refund reversals? |
```

---

## Step 10 — Generate Orchestration (`generate.rs`)

New page-writing loop (replaces flat per-community loop):

```
1. Group communities by feature → FeatureGroup[]
2. Generate dev slug per community (primary class, dedup)
3. For each feature:
   a. Write pages/<feature>/index.md
   b. Write pages/<feature>/po.md
   c. Write pages/<feature>/ba.md
   d. For each community in feature:
      Write pages/<feature>/dev/<slug>.md
4. Write pages/index.md
5. Write pages/open-questions.md  (if --llm and any open questions)
6. Write pages/routes.md + openapi.json (unchanged)
```

The LLM enrichment phase runs after grouping and before writing:
- Feature calls: 18 concurrent (pool of 8) → `FeatureLlmDoc` per feature
- Community calls: 351 concurrent (pool of 8) → `CommunityLlmDoc` per community

---

## Step 11 — Manifest (`manifest.rs`)

Update page path format from `po/service-community-13` to `payment/dev/permission-service`.
Add optional fields:

```rust
pub llm_mode: Option<String>,          // "full" | "legacy-enrich"
pub llm_model: Option<String>,
pub llm_page_count: Option<usize>,
pub evidence_sources: Option<Vec<String>>,
pub llm_failures: Option<Vec<LlmFailure>>,
```

Exit code `3` when `llm_failures` is non-empty but bundle is otherwise written.

---

## Step 12 — `--llm-debug-evidence` and `--llm-dry-run`

`--llm-debug-evidence`: write `<out>/evidence/<feature>/community-<id>.json` after building
packs, before making calls.

`--llm-dry-run`: build packs, print per-feature and per-community token estimates, top-5
largest packs, zero LLM calls.

---

## Test Plan

### `wiki_evidence.rs`
- Feature inference: `modules/payment/` → `payment`; no modules path → `shared`.
- Slug generation: `PaymentController` → `payment-controller`; collision → `payment-controller-2`.
- Test class excluded from slug selection; falls through to next non-test class.
- Evidence pack respects token budget; snippets truncated before BRD chunks.

### `wiki_brd.rs`
- `.md` / `.txt` loaded and chunked at ~400 tokens.
- Unsupported extension → warning, empty result.
- BM25 ranking returns highest-overlap chunks first.
- Glob expansion resolves multiple files.

### `wiki_llm.rs` (use `httpmock`)
- Happy path: mock returns valid JSON → `CommunityLlmDoc` parsed.
- 429 → two retries → success.
- Non-JSON → one retry with "respond with valid JSON only".
- Exhausted retries → `Err`.
- Missing API key → error before HTTP call.

### `cih-wiki` unit tests
- Feature index renders route table and community list.
- Feature PO renders deterministic tables; with `FeatureLlmDoc` adds narrative sections.
- Feature BA renders process steps with file:line; with LLM adds workflow narrative.
- Dev page: `CommunityLlmDoc` narrative appears before deterministic class/method tables.
- Open questions aggregate across features into single page.
- Graph-only mode produces correct pages with no LLM fields.
- Manifest serialises new fields only when present.

### Integration tests (temp 3-feature Java fixture + `httpmock`)
- `wiki` graph-only: correct feature folders created, dev slugs are primary class names.
- `wiki --llm`: pages contain narrative + tables; manifest records mode/model/failures.
- LLM failure on one community: graph-only fallback written, exit code 3.
- `wiki --llm-dry-run`: no HTTP calls, token estimate printed per feature.
- `wiki --llm-enrich`: deprecation warning, pages generated via new code path.
- Slug collision: two services in same feature → `service.md` + `service-2.md`.

---

## Out of Scope for Phase 10b

- `.docx` extraction (stub only, feature-gated)
- PDF extraction
- Anthropic-native API (removed; use OpenAI-compatible endpoint)
- Per-method LLM descriptions
- UI/frontend for evidence citations
