# Architecture & Readability Improvements

Review of the full yummy-cih workspace after Phase 6 (12 Rust crates, ~11.9k Rust LOC,
70 tests at review time).
The codebase has strong bones — correct port/adapter layering, clean dependency graph, locked IDs —
but has accumulated a few issues worth cleaning up before Phase 7+ expands Spring/product APIs.

Overall grade: **B+**. No circular dependencies, no god objects, good test coverage. The items
below are the specific things that should be fixed.

**Implementation status (2026-06-14): completed.** The code now centralizes `NodeKind`
label conversion, seals `ResolveIndex`, splits engine orchestration out of `main.rs`,
extracts server search orchestration, adds the missing Falkor/server tests, and keeps
`cargo test --workspace` passing with 77 tests. Line numbers below are the original
review anchors and may move after the refactor.

---

## Priority 1 — Eliminate Code Duplication

### `NodeKind` string conversion is duplicated

The same 15-arm `NodeKind → &'static str` match exists in **four places**, plus two
string-to-`NodeKind` copies:

| File | Lines | Visibility |
|------|-------|------------|
| `crates/cih-search/src/lib.rs` | 32–50 | `pub` |
| `crates/cih-embed/src/text.rs` | 49–67 | private |
| `crates/cih-embed/src/store.rs` | 389–407 | private |
| `crates/cih-falkor/src/lib.rs` | 463–481 | private |
| `crates/cih-embed/src/store.rs` | 369–386 | private `parse_kind()` |
| `crates/cih-falkor/src/lib.rs` | 483–501 | private `node_kind_from_label()` |

**Fix:** Add label conversion directly on `NodeKind` in `cih-core`:
```rust
// crates/cih-core/src/lib.rs
impl NodeKind {
    pub fn label(&self) -> &'static str {
        match self {
            NodeKind::Class        => "Class",
            NodeKind::Interface    => "Interface",
            // ... 15 arms
        }
    }

    pub fn from_label(label: &str) -> Self {
        match label {
            "Class" => NodeKind::Class,
            "Interface" => NodeKind::Interface,
            // ... 15 arms
            _ => NodeKind::Other,
        }
    }
}
```
Then remove the private/public copies and replace call sites with `node.kind.label()`,
`kind.label()`, or `NodeKind::from_label(raw)`.

**Files to edit:**
- `crates/cih-core/src/lib.rs` — add `NodeKind::label()`
- `crates/cih-search/src/lib.rs` — remove `kind_label()` free function (or delegate to `kind.label()`)
- `crates/cih-embed/src/text.rs` — remove private copy
- `crates/cih-embed/src/store.rs` — remove private `kind_label()` and `parse_kind()`
- `crates/cih-falkor/src/lib.rs` — remove private `node_kind_label()` and `node_kind_from_label()`

---

## Priority 2 — Reduce `ResolveIndex` Over-Exposure

`ResolveIndex` and all ~15 of its query methods (`resolve_type`, `find_member`, `receiver_type`,
`find_member_in_hierarchy`, etc.) are declared `pub`, but **no external crate calls them** — only
`EdgeEmitter` in the same file does. The only true public surface is `resolve_edges()` +
`ResolveOutput`.

**Fix:** Change visibility throughout `crates/cih-resolve/src/lib.rs:36–414`:
```rust
pub(crate) struct ResolveIndex { ... }

impl ResolveIndex {
    pub(crate) fn build(parsed_files: &[ParsedFile]) -> Self { ... }
    pub(crate) fn resolve_type(&self, ...) -> ... { ... }
    // ... all methods → pub(crate)
}
```
Keep `resolve_edges()` and `ResolveOutput` as `pub`.

**Why it matters:** Callers in other crates currently could depend on the unstable internal API.
Sealing it prevents that and makes the intended contract (`resolve_edges` is the door) obvious.

---

## Priority 3 — Split `cih-engine/src/main.rs` (1,405 lines)

A single file contains five subcommands + all their orchestration logic. Extract into named modules
with no logic changes (pure rename + move):

| New module | What moves there |
|---|---|
| `src/analyze.rs` | `analyze_emit`, `analyze_from_scope`, `EmitOutcome`, `AnalyzeSummary`, `combined_edges` (~500 lines) |
| `src/discover.rs` | `run_discover_core`, `DiscoverOutcome`, discover helpers (~100 lines) |
| `src/embed.rs` | `run_embed`, `EmbedCommandSummary`, pgvector embedding orchestration (~60 lines) |
| `src/db.rs` | `load_to_falkor`, `LoadOutcome`, batch-load helpers (~200 lines) |
| `src/versioning.rs` | `content_version`, `discover_version`, `latest_graph_artifacts`, `prune_other_versions` (~100 lines) |

`main.rs` shrinks to: CLI struct + `main()` + the `match` arms that dispatch to each module.
Target: < 200 lines.

**Why it matters:** Finding any given function currently requires scrolling through an
1,405-line file. Each module will have a clear scope: `analyze.rs = parse → resolve → emit`,
`embed.rs = nodes.jsonl → pgvector`, `db.rs = load to FalkorDB`, etc.

---

## Priority 4 — Readability Micro-issues

### 4a. `nodes_to_list()` unreadable format string
**File:** `crates/cih-falkor/src/lib.rs:395`

A 105-character single-line format string with 16 fields. Break into named locals:
```rust
let id   = cstr(n.id.as_str());
let name = cstr(&n.name);
// ...
format!(
    "{{id:{id}, name:{name}, kind:{kind}, file:{file}, \
      qn:{qn}, sl:{sl}, el:{el}, props:{props}, \
      stereotype:{st}, httpMethod:{hm}, path:{path}, \
      decorator:{dec}, handler:{handler}, \
      symbolCount:{sc}, cohesion:{coh}, processType:{pt}}}",
)
```

### 4b. BFS O(n) cycle check → O(1)
**File:** `crates/cih-community/src/bfs.rs:35`

`path.contains(next)` is O(path length) for every BFS neighbor. Replace:
```rust
// Before:
let (path_vec, path_set) = &state;  // carry HashSet alongside Vec
if path_set.contains(&next) { continue; }
```
The `Vec<NodeIndex>` is still the result; the `HashSet<NodeIndex>` is dropped after the trace.

### 4c. Remove duplicate `build()` free function
**Files:** `crates/cih-search/src/bm25.rs:34–36`, `crates/cih-search/src/lib.rs:12`

`pub fn build(nodes: &[Node]) -> SearchIndex` just delegates to `SearchIndex::build(nodes)`.
Remove it and stop re-exporting it; callers use `SearchIndex::build` directly. The current server
caller at `crates/cih-server/src/main.rs:215` should become `SearchIndex::build(&nodes)`.

### 4d. Add missing algorithm comments

Five sites have non-obvious logic with no explanation:

| File | Lines | Missing |
|------|-------|---------|
| `crates/cih-search/src/tokenize.rs` | 29–38 | 3-condition camelCase split rules |
| `crates/cih-search/src/rrf.rs` | 6 | `RRF_K = 60` provenance (standard RRF literature) |
| `crates/cih-embed/src/chunker.rs` | 11–16 | chunk_bytes/overlap_bytes parameter semantics |
| `crates/cih-embed/src/text.rs` | 41–47 | why content_hash is truncated to 32 hex chars |
| `crates/cih-embed/src/store.rs` | 149–180 | why small datasets exact-scan instead of HNSW |

One short comment per site is sufficient.

### 4e. Extract helpers from `scan_repo()`
**File:** `crates/cih-engine/src/scan.rs:203–227`

The 132-line `scan_repo()` function mixes file walking, module aggregate building, and JAR
discovery. Extract:
- `build_modules_from_aggregates(agg_map, spring_map)` — lines 203–227
- `discover_and_link_jars(modules, repo_root)` — line 231 + surrounding context

### 4f. Extract server search orchestration
**File:** `crates/cih-server/src/main.rs:155–291`

Phase 6 added the right behavior in the right layer, but `main.rs` now mixes MCP tool methods,
lazy BM25 loading, latest-artifact discovery, semantic-hit conversion, and server startup.
Extract the search-only helpers into `src/search.rs`:
- `QueryArgs` / `QueryResult`
- `semantic_to_search_hit`
- `latest_graph_artifacts_in_dir`
- lazy `SearchIndex` builder, ideally as `SearchState`

Keep the MCP `#[tool]` method in `main.rs`; it should delegate to the search module.

---

## Priority 5 — Add Missing Tests

Two crates still have zero unit tests:

**`cih-falkor`** (570 LOC) — add to `crates/cih-falkor/src/lib.rs`:
1. `node_kind_label_roundtrip` — each `NodeKind` maps to a label and back via
   `NodeKind::label()` + `NodeKind::from_label()`.
2. `cstr_escapes_backslash_and_single_quote` — verifies the Cypher string escaper handles `\`
   and `'`.
3. `risk_from_fanout_buckets` — tests each threshold band of `risk_from_fanout()`

**`cih-server`** (337 LOC) — add to `crates/cih-server/src/main.rs` or the proposed
`src/search.rs`:
1. `direction_parse_unknown_falls_back_to_upstream` — extract the direction parser from
   `impact()` and test invalid string → upstream default.
2. `query_limit_defaults_and_clamps` — extract `args.limit.unwrap_or(10).clamp(1, 50)` and test
   default, zero, high, and normal values.
3. `latest_graph_artifacts_chooses_newest_complete_dir` — small temp fixture with two artifact
   dirs and one incomplete dir.

---

## What NOT to Change

| Item | Reason |
|------|--------|
| Error handling (anyhow in impls, thiserror at ports) | Correct and intentional — documents which layer owns what |
| `cstr()` Cypher escaping | Existing behavior is centralized and covered by the recommended tests; deeper query-builder work is separate |
| `type_id()` panic | Locked invariant guard; changing to `Result` cascades to all callers — separate refactor |
| Community algorithm math (leiden, entry_points, cohesion) | Correct and readable; no changes needed |
| Per-crate test helpers (not shared) | Acceptable at current workspace scale |
| Search as server-layer orchestration | Correct: BM25 over artifacts + pgvector search does not belong in `GraphStore` |

---

## Sequencing

All items are independent — no ordering dependencies:

1. **P1** — `kind_label()` consolidation (touches 4 crates in one logical pass)
2. **P2** — `ResolveIndex` visibility (`cargo check` verifies nothing was missed)
3. **P3** — engine module split (mechanical move, no logic changes; largest change)
4. **P4f** — server search extraction before adding more MCP search features
5. **P4** — remaining micro-issues (can land as one or several small commits)
6. **P5** — tests (add after code is stable)

---

## Verification

```bash
cargo test --workspace                    # all tests still pass
cargo clippy --workspace -- -D warnings  # no new warnings
cargo doc --workspace                    # no private types leaked into public docs
```

---

## Architecture Strengths (do not break these)

- **Clean dependency graph:** cih-core → everything; no reverse deps; no cycles.
- **Port/adapter pattern:** `GraphStore` trait is textbook ports-and-adapters.
  `cih-falkor` is the only impl today; swapping in Neptune/Memgraph later requires only a new adapter crate.
- **Subcommand isolation:** Analysis/discovery use pure cores (`analyze_emit`, `run_discover_core`)
  and defer DB I/O to the end. `embed` follows the same artifact-first pattern, with pure chunking
  and hashing covered in `cih-embed`.
- **Locked IDs:** `method_id()`, `constructor_id()`, etc. in `cih-core` produce stable identifiers
  with immutable schemes. Tests in `cih-core/src/lib.rs:203–235` pin these schemes.
- **Graceful degradation:** Parsers skip unparseable files (never abort a 12k-file scan).
  Phase 6 semantic search falls back to BM25-only when `CIH_PG_URL` is unset, and returns a clear
  error when neither `CIH_ARTIFACTS_DIR` nor `CIH_PG_URL` is configured.
