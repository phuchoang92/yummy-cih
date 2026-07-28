# P1-1 — Analyzer memory: bound the edge-merge peak (first slice)

Status: In progress (2026-07-28). Down-payment on master-plan Phase 6 "bounded
analyzer" (`docs/plans/large-repo-correctness-scale-and-reliability.md`, issues
ANALYZE-01..07), which remains Open overall.

## Context

Review finding **P1-1**: `cih-engine`'s analyzer assembles the whole graph in RAM
before writing (`crates/cih-engine/src/analyze/mod.rs`), and the edge-merge step is a
concrete peak amplifier:

- `combined_edges(&parse_output.edges, &resolve_output.edges)`
  (`crates/cih-engine/src/analyze/merge.rs`) builds a
  `HashMap<(String,String,&str), Edge>` sized `structure.len()+resolved.len()`, which
  **clones every edge** and allocates **two key `String`s per entry** (redundant with
  the `src`/`dst` already inside each `Edge`), then `into_values().collect()` into a
  **second full Vec**. Transient peak ≈ raw inputs + a full cloned copy + the result.
- The caller holds `parse_output.edges` and `resolve_output.edges` (the raw pre-merge
  edges — a full extra copy) alive from the merge all the way to the end of the
  function; they are dropped only at `mod.rs:651-656`, i.e. **after** the six
  whole-graph passes and both artifact writes. So the raw edges sit in the write-phase
  peak for no reason.

On IntelliJ scale (941k edges) this is hundreds of MB of avoidable transient/retained
memory at exactly the worst moment.

**Intended outcome:** cut the edge-merge peak and stop retaining raw edges through the
write phase, with **byte-identical graph output** (proven by an unchanged
`content_version`). This is deliberately a bounded, low-risk slice — not the full
streaming/spilling rewrite.

## Change

### `crates/cih-engine/src/analyze/merge.rs`
Rewrite `combined_edges` to take its inputs **by value** and merge without the HashMap:
```
pub(super) fn combined_edges(mut structure: Vec<Edge>, resolved: Vec<Edge>) -> Vec<Edge>
```
- `structure.extend(resolved)` → one buffer, `structure` first then `resolved` (same
  order the current `.chain()` feeds the map). Inputs are **moved**, not cloned.
- **Stable** `sort_by` on `(src, dst, kind.cypher_label())` — groups duplicates while
  preserving the structure-before-resolved order the stateful merge depends on.
- `dedup_by` folds each run **in place** into its first element, reusing the exact
  existing merge logic: `merge_call_sites(winner, later)` then, if
  `later.confidence > winner.confidence`, `swap` `later` into `winner` while keeping the
  accumulated props (`let p = winner.props.take(); swap; winner.props = p;`). The swap
  moves (no clone). Result is already sorted by key with one winner per key — identical
  order and selection to the current `into_values() + sort_unstable_by`.
- Peak drops from `inputs + cloned map(+key strings) + result` to a **single moved Vec**
  sorted/folded in place. `merge_call_sites` is unchanged.

Equivalence rests on: (1) stable sort keeps structure-before-resolved and original
order within each source, so a run folds in the same sequence the HashMap processed it;
(2) `dedup_by` always compares against the retained first element of the run (the
evolving `winner`); (3) winners are unique by key, so final order is fully determined by
`(src,dst,kind)` — same as the old trailing `sort_unstable_by`.

### `crates/cih-engine/src/analyze/mod.rs`
- Bind `mut parse_output` (the `ParseScopeOutcome::Parsed { .. }` destructure at ~471).
- Capture `resolved_edge_count = resolve_output.edges.len()` **before** the merge.
- Call `combined_edges(std::mem::take(&mut parse_output.edges),
  std::mem::take(&mut resolve_output.edges))` — the raw edges are freed at the merge
  instead of at end-of-function.
- Remove the now-redundant `drop(std::mem::take(&mut resolve_output.edges))` and
  `drop(parse_output.edges)` at the end (both already empty). Keep `drop(edges)` and the
  `parsed_files`/`skipped`/`unresolved_refs` drops.

### `crates/cih-engine/src/analyze/merge_tests.rs`
- Update call sites to pass owned `vec![..]` (the bench clones per iteration via
  `.to_vec()`); keep the BTreeMap equivalence oracle.
- **Add** a test for the props path (currently untested): `call_sites` accumulate across
  duplicates (capped at 20) and, on a higher-confidence replacement, the accumulated
  props are kept while the winner's scalar fields become the higher-confidence edge's.

## Deferred (still Phase 6 / not in this slice)
Streaming parse→resolve→emit to the artifact writer (blocked today by the six
whole-graph passes — `post_process`, `apply_pattern_rules`, `propagate_loop_depths`,
`emit_similar_to_edges`, `content_version`, `RegistryGraphReport::try_build` — which all
require the full node/edge vectors), disk-backed resolve indexes, per-unit
`parsed_files` release, and an RSS ceiling with a typed skip. These are larger and
riskier; tracked under ANALYZE-01..07.

## Verification
- **Output unchanged (the key gate):** `cih-engine analyze <repo> --all --no-load --json`
  on the same repo before and after must report the **same `content_version`** (it
  hashes all nodes+edges — identical hash ⇒ identical graph). A/B target: this repo
  and/or `crates/cih-engine/tests/corpus/js-cjs-express`.
- `cargo test -p cih-engine` — `merge_tests` (incl. the BTreeMap oracle + new props
  test), `corpus_coverage` (edge/coverage floors), `parse_schema_guard` (unaffected —
  merge is post-parser, no `PARSE_CACHE_SCHEMA` bump).
- `cargo clippy -p cih-engine --all-targets -- -D warnings`; `cargo fmt --all --check`.
- **Memory A/B:** `/usr/bin/time -l cih-engine analyze <repo> --all --no-load` before vs
  after; report max-RSS delta (scales with edge count — modest on this repo, large on
  IntelliJ).
