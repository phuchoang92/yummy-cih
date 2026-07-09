# yummy-cih: Post-Audit Program — fmt gate, structural leftovers, test gaps, analyze perf

## Context

The audit series (PRs #2–#5) closed all known structural debt. Phuc asked "what next" and selected **all four** remaining directions. This plan sequences them as four independent workstreams, each its own branch + PR (the fmt diff especially must not mix with logic changes). Order matters: fmt first so every later diff is format-clean.

Current facts grounding the scope:
- CI's only non-blocking item is `cargo fmt` (`.github/workflows/ci.yml:29-34`, ~150 files drift; tree predates a fmt pass).
- Deferred review items: `leiden_impl/` sitting next to `leiden.rs` in cih-community; layer rules documented in the root `Cargo.toml` comment but not enforced mechanically.
- Test gaps found in the engine review: `decompile`, `embed`, `cmd::taint` have no dedicated tests; service-dependent paths are the reason.
- `docs/plans/analyze-performance.md` (baseline 2026-06-22, 12,334-file banking repo): parse 46% / scan 15% are **Windows-Docker I/O-bound** (Fix 1 = WSL2 filesystem, user-side, no code). Of the code fixes: Fix 2 is half-landed (di_xml.rs parallelized in `f2ed05f`; `integration_xml.rs` still sequential — it was the *bigger* walk at 1m53s), Fix 3 (Falkor batch) already uses `BATCH: usize = 4000` (`crates/cih-falkor/src/lib.rs:29`), Fix 4 (resolve parallelization) targeted the legacy emitter we deleted and is 1% of runtime — skip per the measurement discipline that killed the interning experiment.

## WS1 — fmt normalization + blocking gate (branch `fmt-gate`)

1. `cargo fmt --all` — one mechanical commit, nothing else in it.
2. Verify: `cargo test --workspace` + `cargo clippy --workspace --all-targets -- -D warnings` (fmt can reflow code but never changes semantics; the gates confirm).
3. Second commit: ci.yml — make the fmt step blocking (`cargo fmt --all --check`, drop `|| true` and the TODO comment); update the CLAUDE.md lint-gate paragraph ("fmt non-blocking" note goes away).
4. Check for a `.rustfmt.toml` first; if absent, default style (do not introduce config).

## WS2 — structural leftovers (branch `structure-leftovers`)

**A. Dissolve `leiden_impl/`** (cih-community):
- `src/leiden.rs` (driver) → `src/leiden/mod.rs`; `src/leiden_impl/*` → `src/leiden/*`; inner `leiden_impl/leiden.rs` → `src/leiden/core.rs` (avoids leiden/leiden.rs); `leiden_impl/mod.rs`'s module-wide allows + doc (vendored algorithm surface) merge into the new `leiden/mod.rs` header; `src/leiden_tests.rs` → `src/leiden/tests.rs`.
- Mechanical path updates: `crate::leiden_impl::` → `crate::leiden::` across cih-community (lib.rs mod decls included). All moves via `git mv`.

**B. Mechanical layering enforcement:**
- New `scripts/check_layering.py` (~60 lines, stdlib only): parse each `crates/*/Cargo.toml`'s internal `cih-*` deps and assert against the layer map hard-coded from the root Cargo.toml comment (Foundation → Language → Analysis → Storage → Product; each layer may depend only on layers above it). Exit non-zero with the offending edge named.
- CI: add a fast `python3 scripts/check_layering.py` step before the build steps.
- Root `Cargo.toml` comment gains one line pointing at the script.

## WS3 — test-coverage gaps (branch `engine-test-gaps`)

Target pure logic; never require Java decompilers, Postgres, or FalkorDB (CI stays hermetic):
- **decompile**: unit tests for the pure parts — prefix/dir filtering from `cih.decompile.toml` (`decompile_config.rs` parse + matching), tool-selection precedence, output-path derivation. No actual decompiler runs.
- **embed**: `strip.rs` boilerplate-stripping and embedding-text construction are pure — table-driven tests (check what exists first; extend). Postgres paths stay untested/`#[ignore]`.
- **cmd::taint**: the stats structs (`CfgStats`/`PdgStats`) aggregation and refinement-application logic — extract-and-test where a small refactor makes a pure function testable; plus one artifact-fixture test mirroring cih-taint's existing fixture pattern (`crates/cih-taint/src/...` tests write nodes/edges JSONL to a temp dir).
- **CLI args**: extend `cmd/args.rs` parse tests to cover the five Args-struct subcommands round-tripping representative flags (guards clap attribute regressions cheaply).
- Success bar: each named module gains a dedicated test file/module; no service dependencies; workspace suite stays hermetic.

## WS4 — analyze performance, measure-first (branch `analyze-perf`)

1. **Baseline on a real fixture** (fineract, ~4k files, artifacts exist): 3× `cargo run --release -p cih-engine -- analyze . --all --no-cache --no-load --json`, record per-phase timings from tracing lines + artifact hashes (correctness invariant, same method as the dedup PR).
2. **Complete Fix 2**: parallelize `integration_xml.rs` the same way `f2ed05f` did `di_xml.rs` (collect XML file list sequentially, `par_iter` the stateless parse). This was the 1m53s walk on the banking repo.
3. **Artifact-write check** (8% of baseline): look at whether nodes/edges JSONL writing is buffered + whether serialization can use `rayon` chunking; only change if the fineract measurement shows it matters locally.
4. **Falkor batch tuning**: optional, only if the local stack (FalkorDB on 6380) shows load >10% of wall time; try 1000/2000/4000/8000 and keep the winner. Requires services — manual verification, not CI.
5. Re-measure; land what clears >5%; record before/after numbers in `docs/plans/analyze-performance.md` (mark Fix statuses: 2 done, 3 tuned/skipped, 4 skipped-with-rationale — legacy emitter deleted, resolve is 1%).
6. Correctness: artifact hashes byte-identical on fineract + servicemix throughout.

## Verification (common)

- Every WS: `cargo test --workspace` + `cargo clippy --workspace --all-targets -- -D warnings`; WS1 additionally proves the new fmt gate passes on a clean checkout.
- WS4: hash-identical artifacts + recorded timings.
- Copy this plan to `docs/plans/post-audit-program.md` at WS1 start (repo convention).

## Out of scope

- Fix 1 of the perf plan (WSL2 filesystem) — user-environment change on the Windows machine, documented already in the perf plan and the windows-container runbook.
- Resolve-pass parallelization (perf Fix 4) — 1% of runtime, targeted deleted code.
- Roadmap Phase 5 feature work (new languages) — separate product initiative, propose after this program lands.
