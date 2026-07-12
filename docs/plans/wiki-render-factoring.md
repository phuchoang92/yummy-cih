# P2.5a: factor cih-wiki per-page rendering into a standalone render core

## Context

P3.8 (live on-demand wiki serving from cih-server) needs single pages renderable outside the batch pipeline. P1.2 (PageSink, `cc84fc8`) and P1.3 (`--since`, `907bbe1`) have landed, unblocking P2.5a (WIKI_IMPROVEMENT_PLAN.md:101): `render_page(graph, ctx, slug) -> Option<RenderedPage>`, with batch `generate_wiki` becoming a loop over the same core.

**STRICT scope guardrail on every step**: byte-identical `pages/` output including `_category_.json` sidecars, `agent-index.json`, `module_tree.json`; manifest structurally identical modulo `generated_at`. NO cih-server wiring (P3.8), NO WikiGraph interning (P3.8 prereq), NO WikiInput split (deferred — `RenderContext` borrows `&WikiInput`, making the later split mechanical).

**Verified state**: the leaf renderers in `pages/*.rs` are ALREADY pure String-returning fns — the entanglement is in four `emit_*` fns (lib.rs:292/334/756/890) that interleave traversal → enrichment lookup → render → sink.push → PageEntry/NavEntry registration, plus direct fs writes bypassing the sink (`_category_.json`, api dirs, stale-file removal at :661-694/:795-851/:893-918). A proto-context **already exists**: `PageGenCtx` (lib.rs:279-289) threads the global derived maps — P2.5a promotes it. No slug→subject resolver exists (slugs are forward-computed; page kind is implicit in loop position).

**Four load-bearing findings the refactor must preserve exactly:**
1. **`class_dev_slugs` prefix accumulation is byte-load-bearing**: api-flow (api_flow.rs:334-349) and entrypoint (:513-528) pages `filter_map` over the accumulated map — when feature B renders, the map holds features A..B (alphabetical); later-feature classes silently get NO link, earlier-feature classes get a link with a broken relative path. Both must reproduce (solved via `dev_slugs_visible(upto, rendered)`).
2. **Pre-existing nav-overwrite bug** (lib.rs:1387 `nav.extend(ep_batch.nav)` REPLACES a feature's nav with only entrypoint entries): a naive per-page nav append would silently FIX it and change the manifest. Keep per-phase PageBatch merge semantics; file a follow-up ticket for the real fix.
3. **Pre-existing LLM-mode nondeterminism**: `all_method_desc` (:766-773) and `method_flow_desc` (:1082-1113) iterate HashMaps with last-write-wins collisions — enriched page bytes can vary run-to-run. Corpus diff must run **graph-only**. Follow-up ticket to BTreeMap-ify (behavior change — not P2.5a).
4. **RenderedPage needs a JSON-sidecar slot** (dev pages push .md+.json under one PageEntry; routes likewise) and an agent-index slot; controller API index pages get NO PageEntry/NavEntry → `entry: Option<PageEntry>`.

## New module `crates/cih-wiki/src/render.rs` (re-exported from lib.rs)

```rust
pub struct RenderedPage {
    pub rel_path: String,
    pub content: String,
    pub json: Option<(String, String)>,          // dev-class + routes sidecars
    pub entry: Option<PageEntry>,                 // None: controller API index pages
    pub nav: Vec<(String, NavEntry)>,
    pub agent_index: Option<(String, String)>,    // (class_node_id, source_file)
}

pub enum PageSubject {  // positions frozen at index-build time (order-derived today)
    SystemIndex, Routes,
    FeatureIndex{feature}, FeaturePo{..}, FeatureBa{..},
    DevClass{feature, class_id},
    ControllerIndex{feature, controller, position},
    ApiFlow{feature, controller, handler_id, position},
    ScheduledFlow{feature, method_id, position},
    ListenerFlow{feature, method_id, topics, position},
    CommunityIndex, CommunityDetail{community_id}, CommunityPo{..}, CommunityBa{..},
}

pub struct RenderContext<'a> {  // promoted PageGenCtx; what P3.8 holds resident
    pub input: &'a WikiInput<'a>,
    pub feature_groups: Vec<FeatureGroup>,
    // moved from generate_wiki: method_flow_desc (:1082-1144), class_primary_feature
    // (:1259-1309), process_by_handler (:1312), entrypoint counts (:1328-1342),
    // all_method_desc (:766-773), comm_slug_map, enrichment_tier (:438-444),
    // is_safe_page_slug guard (:345-349)
    features: BTreeMap<String, FeatureContext>,   // EAGER (batch is the only consumer now)
    // FeatureContext: class_set + assign_class_slugs output + class_dev_links
    // (:355-430), effective-feature-resolved controllers (:626-657),
    // per-feature scheduled/listener grouping (:775-791)
}
impl RenderContext<'_> {
    /// Replicates the batch's alphabetical-prefix slug accumulation exactly:
    /// union of per-feature slug maps for features <= `upto` in group order,
    /// restricted to `rendered` under --since. None ⇒ all (rendered) features.
    pub fn dev_slugs_visible(&self, upto: Option<&str>, rendered: Option<&HashSet<String>>) -> HashMap<String, String>;
}

pub struct PageIndex { ordered: Vec<PageSubject>, by_slug: BTreeMap<String, PageSubject> }
pub fn build_page_index(graph: &WikiGraph, ctx: &RenderContext) -> PageIndex;
pub fn resolve_slug<'i>(index: &'i PageIndex, slug: &str) -> Option<&'i PageSubject>;
pub fn render_page(graph: &WikiGraph, ctx: &RenderContext, slug: &str) -> Option<RenderedPage>;
// internal: fn render_subject(...) -> Result<RenderedPage> (batch keeps `?` on serialization)
```

Slug convention = manifest slugs; controller index pages get synthetic `{feature}/api/{ctrl_slug}/index` so every file under `pages/` is addressable. **Index-based resolution** (reuses the exact forward assignment fns; collision-proof); `render_page` uses a `OnceCell<PageIndex>` inside RenderContext to keep the 3-arg signature.

## Commits (each: fmt + clippy -D warnings + `cargo test --workspace` green)

1. **`refactor(wiki): promote PageGenCtx to RenderContext with precomputed per-feature state`** — add render.rs; cut-paste (NOT rewrite — byte discipline) the derived-state builders into `RenderContext::build`; emit fns consume `ctx.features[feature]`; driver's `class_dev_slugs` accumulator fills from `FeatureContext::slug_for`; add `dev_slugs_visible`. Copy this plan to `docs/plans/wiki-render-factoring.md` in this commit.
2. **`refactor(wiki): hoist filesystem sidecar writes out of page emitters`** — move all direct fs I/O from the emit fns into `generate_wiki`, derived from RenderContext; apply the same `--since` affected-features filter that gated `emit_feature_section` (entrypoint sidecars unfiltered, matching :1384); api `_category_.json` written when the feature has controllers OR entrypoints (contents identical in both current paths). Emit fns become fs-free.
3. **`refactor(wiki): render pages through RenderedPage and a per-subject core`** — `render_subject` dispatch to the existing pure renderers (incl. the synthesized-node dev fallback :557-585 and the community `processes_here` filter :920-932); emit fns become thin subject loops; `PageSink::push_page(&RenderedPage)` adapter (sink internals unchanged); **per-phase PageBatch merge order kept exactly** (global → features → entrypoints → communities) to reproduce the nav-overwrite artifact.
4. **`feat(wiki): page index, resolve_slug, and standalone render_page`** — PageIndex built from the SAME enumeration the batch iterates (batch loops `index.ordered` — one enumeration, structurally impossible to diverge); public re-exports; equivalence tests; tick P2.5a in WIKI_IMPROVEMENT_PLAN.md; file the two follow-up tickets (nav-overwrite fix; HashMap→BTreeMap determinism for LLM-mode builders) as notes in the plan doc.

## Tests

New (tests/wiki.rs):
- **`render_page_matches_batch_output_for_every_page`** (THE acceptance test): synthetic fixture exercising every subject kind — ≥2 alphabetically ordered features, a controller with 2 routes, a **cross-feature call chain** (handler in feature B reaching a class owned by feature A AND one owned by C>B — locks the prefix semantics), scheduled + listener entrypoints, communities with `llm_full`, bodies, flow summaries. Batch-generate; then for EVERY file under `pages/` (not just manifest entries — catches controller index pages), `render_page(slug)` byte-equals the on-disk content (+ json sidecar), and `entry`/`nav` match the manifest.
- `resolve_slug_unknown_returns_none` + per-kind resolution asserts; `page_index_order_matches_manifest_order`.
Existing green unchanged: second-run-zero-writes (:192), `--since` skip test (:227), all pages_*.rs / features.rs / manifest.rs suites, cih-engine crate_tests.

## Corpus verification (minimum after commit 4; ideally after 2 and 3 too)

1. Base-commit binary: `cih-engine wiki` on `/Users/phuc/BigMoves/AI/cih-eval-repos/fineract` (**graph-only mode** — sidesteps the pre-existing LLM-mode nondeterminism) → dir A.
2. Branch-tip binary → dir B. `diff -r A B` excluding `manifest.json`/`wiki_meta.json` → **zero differences** (covers pages/**, `_category_.json`, agent-index.json, module_tree.json). Manifests compared with `generated_at` stripped (jq) → identical INCLUDING the nav-overwrite artifact.
3. Second-run-zero-writes on the corpus with the tip binary; `--since` smoke with a changed-file subset.

## Risks — resolved in design

| Risk | Resolution |
|---|---|
| `class_dev_slugs` prefix/order drift | `dev_slugs_visible(upto, rendered)` replicates the accumulation; cross-feature chain in the acceptance test; Fineract corpus diff |
| Accidentally fixing the nav-overwrite bug | Per-phase PageBatch merge kept verbatim; manifest comparison asserts the artifact; real fix = follow-up ticket |
| Reordering byte-drift | Cut-paste (not rewrite) of derived-state builders; BTreeMap/BTreeSet discipline untouched; per-commit corpus diff |
| `--since` fidelity (partial slug maps, sidecar filtering) | `rendered` param; driver applies the same affected filter to hoisted side effects; existing `--since` test + smoke |
| Batch/index enumeration divergence | Structural: batch iterates `PageIndex.ordered` |
| LLM-mode nondeterminism polluting diffs | Graph-only corpus diff; BTreeMap-ify as a separate ticketed behavior change |
| WikiInput churn / double WikiGraph build | Both deferred: ctx borrows `&WikiInput`; double-build untouched (enrichment path in cih-engine) |

Effort: M (~3d) — commit 1 is the bulk (careful cut-paste of ~250 lines), 2–3 mechanical, 4 small + fixture-heavy test. At implementation start also copy this plan to `/Users/phuc/BigMoves/AI/wiki-render-factoring-plan.md`.

## Implementation outcome (2026-07-12)

**Status: COMPLETE.** Delivered in two commits on `dev`:

- **C1 `9e4944f`** `refactor(wiki): promote PageGenCtx to RenderContext with precomputed per-feature state` — `render.rs` added; `PageGenCtx` promoted to `RenderContext` with eager per-`FeatureContext` state (class sets, slug maps, class-dev links, effective-feature controllers, per-feature scheduled/listener grouping) plus the global derived maps (`method_flow_desc`, entrypoint counts, `all_method_desc`, `comm_slug_map`, `enrichment_tier`). `dev_slugs_visible(upto, rendered)` replicates the byte-load-bearing alphabetical-prefix accumulation. Verified byte-identical on the 15,998-page Fineract graph-mode corpus.
- **C2 (this commit)** `feat(wiki): standalone render_page over a page index` — `RenderedPage`, `PageSubject`, `PageIndex`, `build_page_index`, `resolve_slug`, `render_page`, and the `render_subject` core, all **purely additive**, reusing `RenderContext` and the existing leaf renderers. `resolve_feature_groups(graph, input)` extracted (verbatim move) and made `pub` so both `generate_wiki` and the acceptance test build the same feature set. Public re-exports added. Acceptance test `render_page_matches_batch_output_for_every_page` proves `render_page` reproduces the batch bytes (content + json sidecars) for every enumerated page, and that the enumeration equals the on-disk page files.

**Deviation from the 4-commit plan (deliberate, risk-reducing):** the byte-critical batch loop was left **entirely untouched** rather than being rewritten to route through `render_subject`. `render_page` is proven equivalent by the acceptance test + corpus diff instead of by shared code. This makes C2's byte-identity *structural* (the batch producing the golden output never changed) at the cost of a temporary second rendering path. Consequences ticketed below. Because the batch was untouched, the planned C2 (hoist fs sidecar writes) and C3 (route batch through `render_subject`) became unnecessary as P2.5a prerequisites and roll into the batch-unification ticket.

Corpus re-verification (C2 vs C1 binary, Fineract graph mode): `pages/**` byte-identical, `manifest.json`/`module_tree.json` identical modulo `generated_at`, `agent-index.json` differs only in the `wiki_dir` output-name field. Full `cargo test -p cih-wiki` (42 tests) + `clippy --all-targets -D warnings` green.

## Follow-up tickets

- **FT-1 batch-unification** (was C2+C3): route `generate_wiki`'s emit loops through `render_subject`/`RenderedPage` and hoist the direct fs sidecar writes (`_category_.json`, api dirs, stale-file removal) into the driver, eliminating the second rendering path. Guarded by the existing acceptance test + corpus diff. Non-trivial byte discipline; deferred because P2.5a (standalone `render_page` for P3.8) no longer needs it.
- **FT-2 nav-overwrite bug** (lib.rs `nav.extend(ep_batch.nav)` REPLACES a feature's nav with only entrypoint entries): real fix changes the manifest, so out of scope for a byte-identical refactor. `render_page` intentionally reproduces the artifact.
- **FT-3 LLM-mode determinism**: `all_method_desc` and `method_flow_desc` iterate `HashMap`s with last-write-wins collisions → enriched page bytes can vary run-to-run. BTreeMap-ify (behavior change). Corpus verification stays graph-only until fixed.
