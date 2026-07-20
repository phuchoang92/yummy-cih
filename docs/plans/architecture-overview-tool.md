# architecture_overview — resolved design decisions

**Status:** design resolved 2026-07-19; **v1 implemented 2026-07-19**
(`application/architecture_overview.rs` + `transport/mcp/overview.rs`). The overview
ships the fix-A provenance vocabulary internally (per-section `source`, hoisted
clocks, backfill-shaped warnings); fix A proper — labels on `status`/`route_map`/
wiki responses and registry-stat backfill at load time — remains open and should
reuse the same vocabulary. Test strategy items (a)+(b) are implemented (fake
store + embedded-ladybug end-to-end via a cih-server dev-dep, both hermetic),
and the falkor `--ignored` smoke (`falkor_fineract_overview_presence_and_labels`)
passed against the live 87k-node fineract graph well under the 2s budget —
empirically confirming D2's live-first latency call.
**Provenance:** one architect brainstorm + a two-expert resolution panel
(solutions-architect and AI/agent-consumer lenses), both grounded in this tree at
`fe31c96`; the one disputed point (hotspots) was decided by the product owner.
Full transcripts live outside the repo in the Codex workspace
(`2026-07-19/cih-fineract/outputs/`: `cih-mcp-improvement-proposal.md`,
`architecture-overview-design-brainstorm.md`); this record is self-contained.

**Problem being solved:** orienting in a large indexed repo (fineract: 87k nodes,
962 routes, 39 communities) takes 8+ chained narrow MCP calls with truncated and
mutually contradictory output. One call should return a compact, labeled,
size-capped orientation that seeds the narrow tools.

## Resolved decisions

### D1 — Surface: tool only

`architecture_overview(repo?: string, sections?: string[], limit?: int)` as a new
tool router (`crates/cih-server/src/transport/mcp/overview.rs`) over a shared module
(`crates/cih-server/src/overview.rs`). No MCP resource in v1: agents choose from
the tool list and rarely enumerate resources, and `resources::read_resource` is a
stateless filesystem reader by design (`resources.rs:141`) — a live-graph resource
would break that boundary. A `cih://repo/{name}/overview` resource mirror is
**demand-driven and unscheduled** (see Phasing), not "v2 planned".

### D2 — Composition: live at call time, existing port methods only

Compose over the `GraphStore` port surface that already exists —
`graph_summary()`, `graph_overview()`, `communities()`, `community_graph()`,
`route_map()`, `complexity_hotspots()` (`cih-graph-store/src/lib.rs:247-321`) —
plus labeled file reads (registry, entrypoints sidecar, wiki index). **No new
port method** (every addition costs both adapters + contract-suite cases) and
**no `overview.json` artifact**. Grounds:

- The motivating bug *is* a stale precomputed snapshot (registry `routes: 0` vs
  wiki 962; stats only filled by discover, `cih-engine/src/registry.rs:45-46`).
  Precomputing the overview would add a fourth clock.
- The wiki already moved to live rendering with the on-disk bundle demoted to
  fallback (`wiki.rs::live_index_for`, `wiki.rs:437`) — precompute would swim
  against the project's own trajectory.
- Discover's graph load is best-effort (`LoadOutcome::Skipped|Failed` are normal,
  `discover.rs:799-801`), so a discover-time artifact can describe a graph that
  was never loaded. Live composition cannot.
- Latency is a non-issue: the `/graph` browser serves these aggregates
  interactively today; taint runs 4 phases over ~46k nodes in 0.77s
  (`docs/ARCHITECTURE.md`).

### D3 — Sections

Default set: **`stats`, `modules`, `route_groups`, `entrypoints`, `wiki_pages`**,
plus mandatory `provenance` and `warnings`. **`hotspots` is opt-in** via
`sections=["hotspots"]` — product-owner decision 2026-07-19: complexity data
during orientation invites refactoring detours; it stays one call away.

| Section | Backing | Notes |
|---|---|---|
| `stats` | `graph_summary()` | Renamed from "summary" — that word primes LLMs for prose |
| `modules` | `communities()` + **2–3 `anchor_symbols` per row** | Canonical NodeIds of top-degree members; `CommunityInfo` alone (`{id,name,symbol_count,cohesion}`) is a dead end an agent can't feed to `context()`/`impact()`. Field docs say "detected clusters", not "modules-as-fact" |
| `route_groups` | `route_map()` via the **port** | Bucketed by path prefix; samples carry trace_flow-ready `Route:METHOD /path` ids + full `handler_id` |
| `entrypoints` | `.cih/entrypoints.json` + `graph_overview()` degrees | Sidecar absence is ambiguous — see risk 2 |
| `wiki_pages` | **live wiki index** (bundle fallback) | Slug + title pointers only, ~10 tokens each; never inline wiki prose (LLM-enriched, possibly older graph_version). Must resolve through the same path `get_wiki_page` uses or slugs can dangle |
| `hotspots` (opt-in) | `complexity_hotspots()` | |

Every id emitted anywhere uses the exact `Kind:qualified.Name` NodeId form the
other tools accept — agents copy-paste ids; format mismatch = failed call +
disambiguation detour.

**Degradation:** a requested section always appears. Pipeline step not run →
`{"available": false, "reason": "discover has not run for this index", "remedy":
"cih-engine discover <repo>"}` — wording must never read as "none found" (agents
will otherwise report "this repo has no modules" as fact).

### D4 — Freshness: serve always, label always, warn loudly

- Never refuse for staleness; hard error only for unknown repo.
- Per-section label: **`source` only**, one word —
  `graph | registry | artifact | wiki-live | wiki-bundle`.
- All timestamps hoist into the single `provenance` block (one clock per source:
  registry `indexed_at`/`git_head`, wiki `graph_version`/`generated_at`, artifact
  mtimes). No call-time timestamps in section bodies — responses stay
  byte-stable for prompt caching, and agents don't diff ISO strings anyway.
- Staleness verdicts are computed **server-side** and shipped as imperative
  `warnings` strings with remedies (e.g. "wiki describes an older index
  (graph_version mismatch) — prefer graph-sourced counts; regenerate:
  `cih-engine wiki <repo>`").
- **Skew cross-check:** there is no in-store version marker, and loads are
  skippable/failable (D2) — so "graph, as of now" cannot name its artifacts
  version. v1: compare `graph_summary()` totals against registry `stats.nodes/
  edges`; warn on gross mismatch ("store contents may not match latest artifacts
  — reload"). Long-term: write a version marker at `bulk_load`/`publish_to`
  (port change + contract-suite case — deliberately not v1).

### D5 — Size budgeting

- Per-section default caps tuned to **~2k tokens total** (orientation output
  lives in context all session; `limit` can raise it).
- `limit` = plain **max items per list**, clamped per section — not a
  proportional multiplier (unspecifiable contract).
- Hard **32KB backstop**: drop whole trailing sections in the declared priority
  order (`stats` kept first; opt-ins dropped first); the warning names dropped
  sections with the exact re-fetch call. Ordering is normative (golden tests).
- Every `truncated: true` carries `total` + a copy-pasteable `next` hint in
  exact tool-call syntax (`"next": "route_map(prefix=\"/loans\")"`); a test
  validates every emitted hint template against the live tool router — a hint
  that drifts from a real signature teaches hallucinated calls.
- **JSON only** in v1, emitted via `json_result`. **No fix-C coupling**: fix C
  lands at the single `json_result` choke point (`utils.rs:19-23`) and the
  overview inherits `structuredContent` automatically. Define the serde
  `OverviewResponse` struct from day one (future output schema, golden-test
  anchor). `format:"markdown"` is v2, rendered from the same struct.

### D6 — Group mode: per-repo + thin `group` section

The overview stays per-repo (`repo` arg → `resolve()`, default primary). Append a
`group` section populated via `groups_containing(repo)` exactly as `status`
builds it (`app.rs:429-448`) — one semantic for group membership, and it works
even when the server isn't group-fronted. Member rows: the exact copy-pasteable
`repo` string, one-line post-fix-A registry stats, `contracts_synced_at`/`stale`.
Cross-repo edges appear as counts + a pointer to `group_contracts`, never merged
content. A merged `group_overview` is demand-driven v3 at earliest.

## Overturned claims from the earlier brainstorm

Recorded so they don't resurface:

- **"Route grouping is capped at 1000 until fix B" — wrong.** The 1..1000 clamp
  is tool-level (`app.rs:384`); the port method takes a bare `usize` passed
  straight to the query `LIMIT` (`cih-falkor/src/query.rs:551-560`). The overview
  composes over the port and sizes its limit from the live Route count. Complete
  grouping has **no fix-B dependency**.
- **"First adopter of fix C" — dropped.** Needless cross-fix sequencing; adoption
  is automatic at the choke point.

## Definition of done (v1) — beyond the code

Instruction shadowing is the top adoption risk: agents weight server
`instructions`, CLAUDE.md, and workflow docs above `tools/list` descriptions.
Shipping the tool while the old 7-step workflow still stands means agents keep
making 8 calls. The **same release** must include:

- `get_info` core workflow updated — `architecture_overview` becomes step 2 after
  `list_repos` (`app.rs:705-712`).
- `docs/agent-workflows/exploring.md` rewritten around the tool (it is currently
  the manual multi-step version of it).
- CLAUDE.md tool-table entry.
- Router-count guard bumped 29→30 (`app.rs:775-779`) + `dispatch_tests.rs` entry.

Tool description draft (agent-facing; start from this verbatim):

> One-call architectural orientation for an indexed repo. Returns compact,
> size-capped sections: stats (per-kind node/edge counts), modules (detected
> module clusters with anchor symbol ids), route_groups (endpoints bucketed by
> path prefix, with trace_flow-ready sample routes), entrypoints (schedulers,
> listeners, high-degree hubs), wiki_pages (slugs for get_wiki_page), plus
> provenance and warnings. Call this FIRST after list_repos when orienting in an
> unfamiliar codebase — it replaces chaining status/communities/route_map/
> search_wiki. Call it once per repo per session; go deeper with the narrow
> tools it points to (context on an anchor symbol, trace_flow on a sample route,
> route_map(prefix=...), get_wiki_page(slug=...)) rather than re-calling with a
> larger limit. Truncated lists carry total + a ready-to-use `next` call; a
> section with "available": false means a pipeline step has not run (its
> `remedy` says which command) — it is NOT a fact about the codebase. Optional:
> sections=[...] to select sections ("hotspots" is opt-in), limit to scale list
> sizes, repo to target a non-primary repo.

## Risk register

1. **Store-vs-artifacts skew is undetectable** (no in-store version marker;
   loads best-effort). Mitigated by the D4 cross-check; long-term version marker
   is a port change with contract coverage.
2. **Entrypoints sidecar lifecycle**: `.cih/entrypoints.json` is unversioned and
   `write_entrypoints_sidecar` returns without writing when records are empty
   (`discover.rs:862-864`) — absence is ambiguous and a stale file survives a
   re-discover that finds none. Overview must disambiguate via registry
   `community_artifacts_dir` + file mtime as `as_of`. **Independent engine fix
   recommended**: always write (even `[]`) with a `source_version` field.
3. **Torn composition under concurrent publish**: the overview issues ~5–6
   queries; a `publish_to` mid-call yields a cross-version response.
   Document-only in v1 (optionally re-read `graph_summary` totals at the end and
   warn on change); no locking.
4. **Ladybug test gating**: cih-server gates ladybug behind an opt-in feature —
   driving `overview.rs` through ladybug in cih-server tests requires adding
   `cih-ladybug` as a **dev-dependency** (dev-deps don't leak into release
   builds).
5. **Error taxonomy**: `Backend` error on the first query (`graph_summary`) =
   hard error (store down). `Backend` on later sections = per-section `error`
   reason **distinct from** "step not run" — otherwise an outage masquerades as
   "discover never ran" and the degradation design misleads.
6. **Agent-behavior risks**: overview-as-crutch re-calls ("call once per
   session" in the description; cap `limit`); `available:false` misread as
   codebase fact (wording rule in D3); vocabulary collisions (stats, "detected
   clusters", "runtime entry points: schedulers/listeners/hubs").
7. **Fix-C double payload**: if fix C emits `structuredContent` *alongside* text
   JSON, this tool — the largest response on the server — doubles token cost in
   clients that surface both. Decide alongside-vs-instead in fix C before this
   tool ships.

## Test strategy

- **Hermetic (bulk):** `overview.rs` as free functions over `&dyn GraphStore` +
  artifact paths. (a) Fake store for shaping logic: caps, `limit` clamping,
  `sections` filtering, deterministic ordering, byte-backstop drop order,
  degradation when discover artifacts are absent. (b) Embedded ladybug loaded
  from a corpus fixture (`crates/cih-engine/tests/corpus/js-cjs-express` or the
  spring fixtures) end-to-end, in `cargo test --workspace` (needs the dev-dep,
  risk 4).
- **Label assertions**: every count carries `source`; analyzed-but-not-discovered
  repo yields `modules.available:false` + remedy, never a bare zero.
- **Falkor integration** (`--ignored`, like `falkor_integration`): indexed
  fineract — ≥1 module with anchor symbols, ≥1 route group, stats labeled
  `source:"graph"`, response under the byte cap, <2s wall. Presence-and-labels,
  never exact counts (index-version-dependent).
- **Guards**: router-count 30, `dispatch_tests.rs` entry, hint-syntax validation
  against the tool router; `structuredContent`-vs-`OverviewResponse` schema
  assertion once fix C lands.

## Phasing

- **v1** (after fix A): the six decisions above. One new module + router; no
  engine changes, no port changes, no new artifacts. Collapses ~5 of the 8
  fineract orientation calls into one.
- **v2**: `dependencies` (`community_graph()`), `data`/`events` detail (DbTable/
  KafkaTopic/MessageDestination rankings), `format:"markdown"` (rendered from
  `OverviewResponse`, never from wiki text), `processes` pointers (named flows —
  domain verbs are the densest orientation tokens in the graph).
- **v3 (demand-driven only)**: taint/security summary from cached artifacts;
  merged `group_overview`; resource mirror + `overview.json` artifact (a bet on
  resource-consuming clients — revisit only with evidence).
