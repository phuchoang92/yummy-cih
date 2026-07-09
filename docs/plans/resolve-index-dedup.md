# Plan: Unify cih-resolve's duplicate index/emit implementations

## Context

`cih-resolve` carries two parallel implementations of its core resolution machinery:

| | Legacy | Production |
|---|---|---|
| Index | `src/index.rs` — `ResolveIndex` (450 lines) | `src/common/index.rs` — `CommonIndex` (579 lines) |
| Emitter | `src/emit.rs` — `EdgeEmitter` (674 lines) | `src/common/emit.rs` — `EdgeEmitter` (705 lines) |
| Entrypoint | `resolve_edges()` — lib.rs:102, self-described as "Backward-compatible entrypoint (uses old ResolveIndex for tests)" | `resolve_with_registry()` — lib.rs:109, **the only entrypoint the engine calls** (`cih-engine/src/analyze/mod.rs:374`) |
| Consumers | `src/tests.rs` (5 direct `ResolveIndex::build` sites) and `tests/resolve.rs` (20 `resolve_edges` calls, 1,332 lines, ~68 asserts) | all 15 `lang/*` resolvers + lib.rs |

Two problems:

1. **~1,124 lines of drifting duplicate.** `CommonIndex` is a superset (adds confidence scoring via `resolve_type_with_confidence`, registry-driven language dispatch, `resolve_type_in_language`); the legacy pair no longer runs in production but must be kept compiling.
2. **The big test suite points at the dead path.** `tests/resolve.rs` — the crate's main behavioral suite — exercises legacy `resolve_edges`, so the production `CommonIndex` path is tested only indirectly through `cih-engine`'s integration tests.

Also, both modules being named `index`/`emit` (with `common/` saying nothing about the distinction) was flagged in the naming review as the workspace's worst comprehension hazard.

**Goal:** one implementation, tested by the main suite, with unambiguous names. Production behavior (`resolve_with_registry` output) must be provably unchanged.

## Approach

### Phase 1 — Repoint `resolve_edges` at the production path

In `crates/cih-resolve/src/lib.rs`:

```rust
/// Convenience entrypoint: resolve with the default language registry.
pub fn resolve_edges(parsed: &[ParsedFile]) -> ResolveOutput {
    resolve_with_registry(parsed, &default_registry(), ResolveOptions::default())
}
```

(`default_registry()` exists at lib.rs:91; check `ResolveOptions<'_>` derives/impls `Default` — it's a one-field-ish struct at lib.rs:82; add `Default` if missing.)

Then `cargo test -p cih-resolve` and **triage every `tests/resolve.rs` failure individually**:
- Divergence is an *improvement* (CommonIndex resolves more, or adds confidence) → update the assert, note it in the commit message.
- Divergence is a *regression* (legacy resolved something CommonIndex misses) → fix `CommonIndex`, not the test. These are the valuable finds; do not paper over them.

This phase is the bulk of the work. Expect edge-count and confidence-value churn; the ~68 asserts are mostly structural (edge src/dst/kind) so most should pass unchanged.

### Phase 2 — Migrate the unit tests

`src/tests.rs` (294 lines): replace the 5 `ResolveIndex::build(&files)` sites with `CommonIndex::build(&files, &default_registry())`. API is method-compatible (`resolve_type`, `find_member`, `supertypes`, `implementors` all exist on `CommonIndex` as `pub`). Delete the `#[cfg(test)] implementors` shim on the legacy index if it's the last reference.

### Phase 3 — Delete the legacy pair

- Delete `src/index.rs` and `src/emit.rs`; remove `mod emit; mod index;` from lib.rs (lines 32–33).
- `cargo clippy --workspace --all-targets -- -D warnings` will catch any straggler references (the workspace gate is blocking now).

### Phase 4 — Hoist `common/` and reclaim the good names

- `git mv src/common/index.rs src/index.rs`, `git mv src/common/emit.rs src/emit.rs`; move `src/common/inheritance.rs` up too (or into `index.rs` if small); dissolve `common/mod.rs`.
- Update `use crate::common::index::CommonIndex` → `use crate::index::CommonIndex` across `lang/*` (15 files, mechanical sed) and lib.rs.
- Final commit: rename `CommonIndex` → `ResolveIndex` (the good name, now unambiguous). Keep this as its own commit so the move and the rename diff separately.

## Invariant & verification

**Production output must not change.** `resolve_with_registry` itself is untouched in every phase — only tests and the convenience entrypoint move.

- Before starting: `cargo run --release -p cih-engine -- analyze <fixture> --no-cache` on `cih-eval-repos/fineract` (or servicemix); save `edges.jsonl` + `nodes.jsonl` line counts and a sorted hash (`sort edges.jsonl | blake3sum`).
- After Phase 4: repeat; artifacts must be byte-identical (modulo version stamp). Any diff means a phase touched production code — investigate before proceeding.
- Per phase: `cargo test --workspace` + the workspace clippy gate.
- Note for Phase 1 triage: legacy-vs-common behavioral diffs found there do not violate the invariant (they only affect `resolve_edges` callers, i.e. tests), but each CommonIndex *fix* made in triage must be re-verified against the artifact diff at the end.

## Out of scope

- The other naming-review leftovers (`leiden_impl/` restructure, `cih-engine` `cmd/` consolidation) — separate, unrelated churn.
- Any behavior change to `resolve_with_registry` beyond regressions found in Phase 1 triage.

## Effort

Phases 2–4 are mechanical (~1–2 h total). Phase 1 is discovery-bound: if the suites diverge heavily, budget a day; if CommonIndex truly supersedes legacy, it's an hour. Land as 4 commits matching the phases.
