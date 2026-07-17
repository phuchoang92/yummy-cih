# Plan — Improve yummy-cih MCP read-path (query/tool) latency

## Progress (2026-07-16, branch `perf/read-path-latency`)

Measured on fineract (87,739 nodes) via `cargo run -p cih-falkor --example query_bench`;
baseline committed at `docs/perf/read-path-baseline.json`.

- **[DONE] Item 0 — measurement harness + baseline.** New `cih-falkor/examples/query_bench.rs`
  times the graph-backed read tools (median/p95 JSON); baseline file committed.
- **[DONE] Item 1 — `n.name` FalkorDB index.** Added to `ensure_schema` + the bulk path.
  EXPLAIN went `Node By Label Scan` → `Node By Index Scan`; `candidates_by_name` warm
  latency ~7ms → **0.9ms** median (and now O(log n), so it scales with graph size).
- **[DONE] Item 2 — BM25 inverted index.** `SearchIndex`/`TextIndex` now precompute a
  postings list (term → [(doc, tf)]) at build time instead of rebuilding a per-doc
  frequency map on every query; `search.rs` caches the index behind `Arc` (no per-query
  clone). Byte-identical output guarded by a new parity test against a naive reference
  (`inverted_index_matches_naive_reference`).
- **[DONE] Item 5 — concurrent `context` + `detect_changes`.** `context`'s 4 independent
  sub-queries now run under `tokio::try_join!` (**4.23ms → 1.89ms**, ~2.2×). `detect_changes`'s
  per-node `impact` fan-out (up to 20 changed nodes) now runs via a `tokio::task::JoinSet`
  instead of a serial loop (**7.43ms → 2.28ms** for 20 nodes, ~3.3×; wider on larger blast
  radii). Behavior preserved — same node selection, error-swallowing, and set-derived output.
- **[DONE] Item 3 (core) — registry cache.** `Registry::load_cached()` (mtime-invalidated
  `Arc` snapshot) added in cih-core; the every-tool-call `resolve_repo_entry` path uses it.
  Remaining lower-frequency `Registry::load()`/`GroupRegistry::load()` sites in
  `app.rs`/`contracts.rs` (list_repos, status, contract tools) are follow-ups.
- **[DONE] Item 4 — taint_paths / shape_check artifact cache.** New `artifact_cache.rs`
  (`ArtifactCache`, mtime-invalidated `Arc<ArtifactBundle>`, mirroring `XflowState`) holds
  the raw file-ordered nodes+edges; `taint_paths` and `shape_check` load through it instead
  of re-reading `nodes.jsonl`+`edges.jsonl` per call. On fineract the eliminated per-call
  parse is **~1.4s** (nodes 617ms + edges 770ms → Arc clone on a hit; measure with
  `cargo run -p cih-server --example artifact_load_bench`). Correctness preserved — all
  taint + contract tests pass; node/edge ordering is unchanged (raw bundle, not the
  id-keyed `ArtifactGraph`).
- **[DONE] Item 0 (timing log) — env-gated per-tool timing.** `#[tool_handler]` on
  `impl ServerHandler for CihServer` replaced with a hand-written `call_tool` (verbatim
  rmcp-0.7 expansion + timing) that logs `tool_call { tool, repo, elapsed_ms, ok }` when
  `CIH_TOOL_TIMING=1`/`true`, silent otherwise. Verified end-to-end over MCP Streamable
  HTTP: enabled → one `tool_call` line per call (`tool=list_repos elapsed_ms=3 ok=true`),
  disabled → none, dispatch unchanged in both.
- **[DONE] Item 3 (remainder) — cache remaining registry reads.** Added
  `GroupRegistry::load_cached()` (twin of `Registry::load_cached()`) and routed the
  read-only `list_repos` / `status` / `group_freshness` / `api_impact` / `trace_flow_x` /
  `shape_check` sites through the cached snapshots. Mutating engine/CLI paths stay on
  `load()` + `save()`.

---


## Context

`yummy-cih` is used interactively: an agent fires MCP tool calls (`context`,
`impact`, `trace_flow`, `search_code`, `taint_paths`, …) against a live graph on
every question. The **indexing/analyze path is already heavily optimized** (native
`GRAPH.BULK` ~30×, parallel artifact serialization, per-file parse cache) and its
remaining hot spots are small or already measured-and-rejected — see
`docs/plans/analyze-performance.md`. The **read/serving path has never been
optimized** and carries several avoidable per-call costs that hit every interaction.

This plan targets **MCP tool-call latency** (the read path in `cih-server` /
`cih-falkor` / `cih-search`), with lightweight **machine-readable timing + a
committed fineract baseline** so each change is proven before/after — matching the
repo's measure-first discipline without building a full CI bench harness.

Correctness invariant for every read-path change: tool responses must be
**semantically unchanged** (same nodes/edges/hits, same ordering where ordering is
defined). Verify by diffing tool output before/after on fineract.

## Findings (all code-verified)

| # | Cost | Where | Impact |
|---|------|-------|--------|
| A | `n.name` **not indexed** → full `:Symbol` label scan on every short-name resolution | `cih-falkor/src/query.rs:23` (`ensure_schema`), `lib.rs:225,391-392` | Every `context`/`impact`/`trace_flow`/`find_duplicates`/`test_coverage` call that passes a bare name (`symbol.rs:53-56` → `candidates_by_name`, `query.rs:535-541`) |
| B | BM25 `search` rebuilds a per-doc `term_freq` HashMap **for every doc on every query**; cache hit `.clone()`s the whole index | `cih-search/src/bm25.rs:98-102`; `cih-server/src/search.rs:97` | Every `search_code`/`query`/`feature_map`; O(N_docs × tokens/doc) per query |
| C | `latest_in_dir` does a `read_dir` + per-subdir `stat` **on every search query** even on cache hit | `cih-server/src/search.rs:90` (`artifacts.rs:63-101`) | Every search query |
| D | `Registry::load()` reads+parses the registry file **on every tool call** | `cih-server/src/utils.rs:52-53`; also `GroupRegistry::load()` in `contracts.rs` | Every tool call |
| E | `taint_paths` and `shape_check` **re-read + re-parse the whole `nodes.jsonl`+`edges.jsonl`** per call, uncached | `taint.rs:101-111`; `contracts.rs:381-386` | Full corpus load per call (fineract ≈ 87k nodes) |
| F | `context` runs **5 sequential** independent Cypher queries; `detect_changes` runs a **sequential N+1** of up to 20 `impact` traversals | `query.rs:455-487`; `changes.rs:59-65` | `context` and `detect_changes` latency |

Existing pattern to reuse for E: `XflowState`/`ArtifactGraph` in
`cih-server/src/xflow.rs:24-47` — an artifacts-dir-keyed, nodes.jsonl-mtime-invalidated
`get_or_load` cache that already builds `nodes_by_id` + in/out adjacency. This is the
"`WikiSearchState::get_or_load` pattern" the codebase already standardizes on.

## Work items (ordered by ROI; each landed + measured independently)

### 0. Machine-readable timing + committed baseline (do first)

- Add a **`cih-server/examples/query_bench.rs`** (mirrors the existing
  `cih-falkor/examples/load_test.rs`): connects to a live fineract FalkorDB graph
  (`FALKOR_URL=redis://127.0.0.1:6380`), runs a fixed representative workload — a
  bare-name `context`, an `impact` (depth 4), a `search_code`, a `taint_paths`, a
  `shape_check`, a `detect_changes` — N iterations each, and prints per-tool
  **median / p95 ms as JSON**.
- Add a lightweight structured per-tool timing log in the tool-dispatch path
  (`cih-server/src/app.rs`): one `tracing::info!` per call with
  `{ tool, repo, elapsed_ms }` (a small timing guard/wrapper), so real usage is
  measurable too, gated behind an env flag (e.g. `CIH_TOOL_TIMING=1`) to stay quiet
  by default.
- Commit a **baseline results file** (`docs/perf/read-path-baseline.json` or a table
  in the plan doc) with the fineract numbers, so every later item records before/after
  against it.

### 1. Add the `n.name` index (finding A) — trivial, broad

Add `CREATE INDEX FOR (n:Symbol) ON (n.name)` alongside the existing id/kind index
creation at **all three sites**: `query.rs:24-25` (`ensure_schema`, the server
first-use path), `lib.rs:225`, `lib.rs:391-392`. `CREATE INDEX` is idempotent, so
existing graphs pick it up on next server start. Measure `context`/`impact` with a
bare name before/after; confirm result set unchanged. Consider a range index on
`n.file` only if the baseline shows `untested_symbols`/`route_map` `STARTS WITH`
scans matter (secondary).

### 2. BM25: precompute term frequencies + stop cloning the index (findings B, C)

- In `cih-search/src/bm25.rs`, compute each doc's term frequencies **once in
  `build()`** and store them on the `Doc` (a `HashMap<String,usize>` or, better, build
  a proper **inverted index**: `term → Vec<(doc_idx, tf)>` so `search()` visits only
  docs containing a query term). Rewrite `search` (`bm25.rs:88-141`) to read the
  precomputed frequencies instead of rebuilding per doc. Apply the same to `TextIndex`
  (`bm25.rs:151-242`) used by wiki search.
- In `cih-server/src/search.rs`, change `CachedIndex.index` to `Arc<SearchIndex>` and
  return the `Arc` on cache hit (remove the `cached.index.clone()` at `search.rs:97`).
- Reduce the `latest_in_dir` dir-stat per query (`search.rs:90`): stat only the
  current `nodes.jsonl` mtime (cheap) rather than scanning every subdir, or gate the
  version recheck behind a short TTL. Lower priority than the two above.

### 3. Cache the parsed Registry / GroupRegistry (finding D)

Hold the parsed `Registry` (and `GroupRegistry`) in `CihServer` behind an
`Arc<RwLock<…>>` with file-mtime invalidation — same `get_or_load` shape as
`xflow`/wiki caches. Route `resolve_repo_entry` (`utils.rs:52`) and the in-body
`Registry::load()`/`GroupRegistry::load()` calls (`app.rs:369,399,402`,
`contracts.rs:43,51,361`) through it.

### 4. Route taint_paths + shape_check through the cached ArtifactGraph (finding E)

Replace the per-call `nodes.jsonl`/`edges.jsonl` load+parse in `taint.rs:101-111`
and `contracts.rs:381-386` with a shared artifacts cache keyed by dir + nodes.jsonl
mtime, reusing/extending `xflow.rs`'s `ArtifactGraph` (`nodes_by_id` + in/out
adjacency already fit taint's `node_meta` + BFS and shape_check's node scans).
`taint_paths` stays on `spawn_blocking`. This is the largest single-call win on large
repos. Keep `refine=true` CFG/PDG source reads as-is (separate concern).

### 5. Parallelize independent queries in context + detect_changes (finding F)

- `context` (`query.rs:455-487`): run its 5 independent queries concurrently with
  `tokio::try_join!` instead of sequentially.
- `detect_changes` (`changes.rs:59-65`): run the up-to-20 `impact` traversals
  concurrently via a bounded `JoinSet` (the `FalkorStore` `query_limit` semaphore
  already backpressures, so this won't overrun FalkorDB).

Deeper Cypher restructuring (the materialize-all-paths-before-`LIMIT` pattern in
`impact`/`flow_downstream`, `query.rs:122-142,626-635`) and the double
string-conversion in `serialize.rs:166-179` are **documented follow-ups**, not in
this plan — revisit only if the baseline flags them after items 1–5.

## Verification

1. **Build/lint gate** (CLAUDE.md): `cargo fmt --all --check`,
   `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`
   (hermetic — no FalkorDB needed).
2. **Correctness**: for each item, diff the affected tool's JSON output before/after
   on fineract — result sets and ordering must be identical (BM25 hits, impact nodes,
   context neighbors, taint paths, contracts).
3. **Timing**: run `cargo run -p cih-server --example query_bench` against a live
   fineract graph (FalkorDB on 6380) before and after each item; record median/p95 in
   the committed baseline file. A change that doesn't move its target tool's number
   didn't do anything — say so and drop it (measure-first discipline).
4. **End-to-end smoke**: start the server, connect via MCP, and exercise
   `context`/`impact`/`search_code`/`taint_paths` on fineract to confirm live behavior.

## Files touched (representative)

- `crates/cih-falkor/src/query.rs`, `crates/cih-falkor/src/lib.rs` — name index (item 1)
- `crates/cih-search/src/bm25.rs` — precomputed TF / inverted index (item 2)
- `crates/cih-server/src/search.rs` — `Arc` cache, mtime-only recheck (item 2)
- `crates/cih-server/src/{app.rs,utils.rs,contracts.rs}` — registry cache (item 3), timing (item 0)
- `crates/cih-server/src/{taint.rs,contracts.rs,xflow.rs}` — shared artifact cache (item 4)
- `crates/cih-server/src/{query.rs,changes.rs}` — concurrent queries (item 5)
- `crates/cih-server/examples/query_bench.rs` (new), baseline results file (item 0)
