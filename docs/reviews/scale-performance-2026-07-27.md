# yummy-cih — Scale & Performance Review

**Date:** 2026-07-27 · **Scope:** scale & performance (read path, search-index memory,
backend readiness, backpressure, analyzer memory) · **Method:** source-grounded; every
headline claim verified against code, not just search.

## Status at a glance

Five findings were fixed on `dev` after this review; three larger items are deferred.

| Finding | Severity | Status | Commit |
|---|---|---|---|
| P2-1 · `route_map`/`communities` hide completeness from the visible MCP payload | Medium | ✅ Fixed | `79936ed` |
| P2-3 · `nodes_in_files` unbounded store-side load feeding `detect_changes` | Medium | ✅ Fixed | `3ce810c` |
| P2-4 · Cursor signing key breaks pagination under scale-out | Medium | ✅ Fixed | `e967919` |
| P1-1 · Analyzer assembles whole graph in RAM (edge-merge peak) | High | ✅ Slice fixed | `c3359d2` |
| P3-e · Adapters silently swallow malformed `call_sites` | Low | ✅ Fixed | `c3359d2` |
| P1-1 · Full streaming/spilling analyzer (Phase 6) | High | ⏸ Deferred | `docs/plans/analyzer-memory-p1-1.md` |
| P1-2 · 256→512 MiB search-cache fix unverified on the real ~409 MiB index | High | ⏸ Deferred (needs artifact) | — |
| P2-2 · Multi-repo hot-set thrash under one 512 MiB search budget | Medium | ⏸ Deferred (needs compact BM25) | — |

---

## Verdict

An unusually disciplined codebase; the *serve/read* side of the scale story is genuinely
well-engineered — hard traversal budgets with a shared BFS kernel, conservative
completeness types, keyed-hash keyset cursors, typed backpressure on two independent
lanes, and a correct FalkorDB loading-readiness gate. Broad signals are excellent: zero
`unsafe`, no `todo!`/`unimplemented!`, no real FIXME/HACK debt, ~1,300 tests, a
backend-neutral store contract suite, corpus coverage floors, idiom ratchets, and
mechanically-enforced hexagonal layering.

No P0. The scale *risk* concentrates on the *analyze/ingest* side (memory) and in open
production-acceptance verification — both tracked candidly in the master plan.

## What's strong (don't regress)

- **Traversal budgets are hard and shared** — `TRAVERSAL_NODE_BUDGET=10_000`,
  `TRAVERSAL_EDGE_BUDGET=50_000`, `EXECUTION_BATCH_SIZE=256`
  (`cih-graph-store/src/lib.rs:16-22`); all bounded ops route through one BFS kernel;
  per-request filters can only lower the ceiling.
- **Truncation is honest** — `ResultBounds`/`Completeness`
  (`cih-server/src/domain/completeness.rs`) never treat a short page as proof of
  exhaustion.
- **Readiness closes PING-while-loading** — FalkorDB `backend_readiness` probes `INFO
  persistence` on a metadata lane (`cih-falkor/src/lib.rs:281-330`); cached single-flight
  monitor; `/health` static & graph-free; every live-graph read tool admission-gated;
  server returns typed `Loading` immediately.
- **Backpressure sheds, never queues unboundedly** — CPU-heavy blocking lane
  (`BlockingError::Saturated`) and the async query lane (`GraphStoreError::Overloaded`),
  both typed-shed.
- **Cursors** — keyed-hash MAC (constant-time), generation-binding, keyset paging,
  bounded page size.
- **Search single-flight** shares one decode of the ~400 MiB index; oversize guard
  refuses insert without evicting healthy tenants.

---

## Findings

### P0 — Critical
None. No silent result corruption, no data-loss path, no unbounded worker blocking, no
`unsafe`.

### P1 — High

**P1-1 · Analyzer assembles the whole graph in RAM before writing.**
`crates/cih-engine/src/analyze/mod.rs` holds all parsed ASTs + the ~13-map `ResolveIndex`
+ `all_nodes` + `edges` + a duplicate `combined_edges` map simultaneously, with six
whole-graph passes before the write. The dominant memory ceiling for IntelliJ-scale repos.
- ✅ **Slice fixed (`c3359d2`):** `combined_edges` rewritten to merge **by value** via
  stable-sort + in-place `dedup_by` (no per-edge clone, no HashMap key strings, no second
  Vec), and the caller moves the raw edge vectors in so they're freed at the merge instead
  of held through the write phase. **Output byte-identical** — proven by an unchanged
  `content_version` (corpus A/B: same `fd9274bb790383d7`, same `edges`/`nodes` shasums).
- ⏸ **Deferred (Phase 6):** streaming parse→resolve→emit, disk-backed indexes, per-unit
  `parsed_files` release, RSS-gate skip — blocked by the six whole-graph passes; planned in
  `docs/plans/analyzer-memory-p1-1.md`. The memory win of the shipped slice scales with edge
  count and is material only at large-repo scale (a decisive large-repo RSS measurement is
  still pending — same class of gap as P1-2).

**P1-2 · The 256→512 MiB search-cache fix is unverified against the real ~409 MiB index.**
The default (`config.rs:72-73`) is sized for the real artifact, but the local IntelliJ
evidence used a ~137 MiB decoded index that fits *both* budgets — so it does not exercise
the change; the decisive run is still pending.
- ⏸ **Deferred:** needs the real `platform`/IntelliJ sidecar + a suitable host
  (`docs/perf/search-platform-474k.md` is "runner ready; target-host evidence pending").

### P2 — Medium

**P2-1 · `route_map`/`communities` hid completeness from the default-visible payload.**
They alone used `json_result_compatible`, putting the bare array in the MCP `content` block
and completeness only in `structured_content`; `route_map` also lacked a `+1` probe.
- ✅ **Fixed (`79936ed`):** both emit via `json_result` (completeness in both blocks); the
  dead helper removed; `route_map` over-fetches by one so truncation is knowable (and now
  reports `complete: true` with an exact total when the full set fits, instead of always
  `complete: false`).

**P2-2 · Multi-repo hot set thrashes under one 512 MiB search budget.**
Two ~409 MiB repos (818 > 512) evict each other — the still-open eight-repo alternating gate.
- ⏸ **Deferred:** the durable fix is the compact ≤230 MiB BM25 representation, not a bigger
  budget.

**P2-3 · `nodes_in_files` did an unbounded store-side load feeding `detect_changes`.**
No `LIMIT`; a huge generated file loaded all matching symbols into RAM.
- ✅ **Fixed (`3ce810c`):** the port method takes a `limit`; both adapters push a bounded
  `LIMIT` (verified in the contract suite); the caller over-fetches by one, truncates, and
  threads a `candidate_load_limit` completeness reason. New `CIH_DETECT_CHANGES_MAX_LOAD`
  (default 5000, ≥ `max_symbols`) bounds memory without touching realistic diffs.

**P2-4 · Cursor signing key breaks pagination under scale-out.**
An unset `CIH_CURSOR_SIGNING_KEY` yields a random per-process key, so replicas reject each
other's cursors (`wrong_key_id`); the old warning only mentioned restarts.
- ✅ **Fixed (`e967919`):** enriched, bind-aware warning naming the replica breakage; opt-in
  `CIH_REQUIRE_CURSOR_KEY=1` hard-fails at startup when the key is absent (mirrors
  `check_auth_posture`); documented in `SECURITY.md`. Non-breaking by default.

### P3 — Low

**P3-e · Adapters silently swallow malformed data.**
`parse_call_sites` did `serde_json::from_str(raw).unwrap_or_default()` — corrupt call-site
evidence vanished with no signal.
- ✅ **Fixed (`c3359d2`):** both adapters now `tracing::warn!` on a genuine parse failure
  (guarded so legitimately-absent `""`/`null`/`[]` don't warn); output byte-identical.

**Assessed — no change warranted:**
- `call_chain` (`[:CALLS*1..12]`) and no-filter `graph_overview` degree scan: both run
  through `rows()` → `run_read()`, which applies a FalkorDB `TIMEOUT` + supervised driver
  deadline (`cih-falkor/src/lib.rs:369-414`), so DB work already sheds with a typed
  `ExecutionTimeout`; output is already capped. A hard budget would change which
  paths/nodes surface for a risk the timeout already bounds.
- `route_map` fields are plain string extraction (no parse to fail); ladybug `communities`
  numerics go through shared `cell_u64`/`cell_f64` helpers (out of scope to touch broadly).
- Ladybug always-`Ready` (embedded, by design); blocking-lane uncancellable at deadline
  (documented trade-off); `Disposer` expansion latency (by-design correctness trade); search
  weight accounting slightly under-counts RSS — all low/by-design; left as-is.

---

## Master-plan status (the four failure classes)

Per `docs/plans/large-repo-correctness-scale-and-reliability.md` (checkpoint 2026-07-26):

| # | Failure class | Status |
|---|---|---|
| 1 | FalkorDB serves while graph is restoring | **Partial** — PING-while-loading defect fixed; publication/epoch/rollback infra open |
| 2 | Search index > retention budget → load/discard cycle | **Implemented locally; production gates open** (P1-2, P2-2) |
| 3 | Legacy reads hide truncation / do whole-graph work | **Implemented locally; corpus gates open** (P2-1 fixed the visible-completeness gap) |
| 4 | Analyzer retains repo-scale structures | **Open** — least progressed; P1-1 slice is a down-payment |

## Open verification (not code — the decisive gaps)

1. **P1-2 / P2-2:** run the real ~409 MiB sidecar through the acceptance runner; confirm
   retained < 90% of 512 MiB with no reload cycle, then the 8-repo alternating gate.
2. **P1-1:** measure analyze-phase peak RSS on IntelliJ to size the streaming/spilling work
   against a target RSS gate.

## Guardrails to keep leaning on
`cargo test --workspace` (hermetic) · `cargo clippy --workspace --all-targets -- -D warnings`
· `python3 scripts/check_layering.py` · the `cih_graph_store::contract` suite ·
`corpus_coverage.rs` floors.
