# Standalone Milestone 1 — offline, DB-free `cih analyze` (Windows-GNU-ready)

## Goal

Ship a `cih` binary whose `analyze` runs fully offline — **no Docker, FalkorDB, or
ONNX/ORT** — and whose dependency tree builds for `x86_64-pc-windows-gnu`. This is
the first milestone of the "cih standalone" proposal and unblocks a usable binary
before the heavy lifts (`LocalGraphStore`, Candle embeddings) land.

**In scope:** `cih` binary; zero-config `cih analyze` (cwd + implicit `--all`);
artifacts-only analyze with no FalkorDB in the tree; Cargo feature-gating of
`cih-falkor` and `cih-embed`.

**Explicitly NOT in scope (later milestones):** DB-free graph *queries* / MCP
(`LocalGraphStore`, M2); Candle / offline model pack / local vector index / semantic
search (M3); dedicated `crates/cih-cli`, `cih doctor`/`serve`/`open`, installer/SBOM
(M4). After M1, graph queries and `serve` still require FalkorDB — `analyze` is what
goes offline.

## Background (verified against the code)

- `cih-engine` has **no `[features]` section**; `cih-embed` and `cih-falkor` are
  unconditional deps (`crates/cih-engine/Cargo.toml:22-23`).
- The engine binary is a thin shim: `main.rs` → `cih_engine::cmd::main()`.
- Coupling to remove is small and fully enumerated:
  - **`cih-falkor` / `crate::db`** — 5 files: `db.rs`, `analyze/mod.rs`,
    `discover.rs`, `cmd/taint.rs`, `cmd/artifact.rs`.
  - **`cih-embed`** — 3 files: `embed.rs`, `feature_strategy.rs`, `discover.rs`.
- `analyze` today loads FalkorDB by default and `exit 3`s on load failure; the
  no-selector case `process::exit(2)` with "Choose a scope" (`analyze/mod.rs:63-68`);
  cwd-default already exists (`cmd/analyze.rs:11-16`).
- `LoadOutcome` (used unconditionally by `print_styled`/`summary`) currently lives in
  `db.rs` — it must be moved out before `db.rs` can be gated (see Step 3).
- Additional falkor-coupled surface beyond the grep above: `run_resolve`
  (`analyze/mod.rs:128`, in the ungated analyze module) and `cmd/refresh.rs`
  (calls `run_discover`, `:12,155`). Both must be gated (Steps 4 and 7).
- `feature_strategy.rs` is shared: the ungated `Features` command uses its
  `make_feature_llm_caller` (`cmd/features.rs:568`) — only its embed parts may be
  gated, never the module.
- Integration tests (`crates/cih-engine/tests/*.rs`) import only ungated modules
  (`analyze`, `scan`, `scope`, `llm`, `file_cache`, `cmd::group_sync`) — no test
  gating needed.
- `DEFAULT_FALKOR_URL`/`DEFAULT_GRAPH_KEY` live ungated in `lib.rs:20-22`;
  `registry::persist_analyze` (which takes `graph_key`) keeps compiling unchanged.

## Design decisions

1. **Standalone = a feature profile, not a fork.** `default = ["falkor","embedding"]`
   keeps the dev/CI build and all current tests byte-for-byte unchanged. The
   standalone binary is built with `--no-default-features`.
2. **Artifacts-only falls out of feature-gating.** With `falkor` off, the DB-load
   code isn't compiled, so `analyze` is artifacts-only with no connection and no
   `exit 3` — no runtime flag needed for that path.
3. **Keep the full build's default = load FalkorDB.** Do not flip the global default
   (that would break the dev/MCP loop). Add an opt-in `--artifacts-only` for the full
   build. Flipping the global default to "write artifacts + update *local* graph" is
   deferred to M2, when a local backend exists.
4. **Compiler-driven gating.** Don't hand-enumerate every `#[cfg]`. Make the deps
   optional, run `cargo build -p cih-engine --no-default-features`, and gate each
   compile error. The list below is the expected set.
5. **Features must be additive — all four combinations compile.** Cargo features
   are unioned across a workspace, so `--features falkor` (no embedding) and
   `--features embedding` (no falkor) must each build, not just "all" and "none".
   Concretely: `discover.rs` uses *both* deps, so it is gated behind `falkor` as a
   module and its embed-strategy path is *additionally* gated behind `embedding`
   with a runtime error fallback ("built without embedding support — use
   `--feature-strategy package`").
6. **Gate the narrowest thing that references the dep, not whole commands, when
   the command has offline value.** `taint` and `artifact export/import` are
   artifact-driven and stay in the standalone build; only their FalkorDB-load
   steps are gated (mirroring analyze). Commands that are *inherently* DB-bound
   (`resolve`, `discover`, `refresh`, `artifact bootstrap`, `embed`) are gated
   wholesale.

## Implementation steps

### Step 1 — Cargo features + optional deps
`crates/cih-engine/Cargo.toml`:
```toml
[features]
default = ["falkor", "embedding"]
falkor = ["dep:cih-falkor"]
embedding = ["dep:cih-embed"]

[dependencies]
cih-falkor = { workspace = true, optional = true }
cih-embed  = { workspace = true, optional = true }

[[bin]]
name = "cih"
path = "src/bin/cih.rs"
```

### Step 2 — `cih` binary
New `crates/cih-engine/src/bin/cih.rs`:
```rust
//! `cih` — standalone product binary. Same dispatch as `cih-engine`; the
//! `cih-engine` bin is retained as a compatibility alias.
fn main() -> anyhow::Result<()> {
    cih_engine::cmd::main()
}
```
Both bins share the package's features, so `--no-default-features` yields the
core-only surface for `cih`. The curated slim surface + `doctor`/`serve` is M4.

### Step 3 — Split `db.rs` so `LoadOutcome` stays ungated
`LoadOutcome` (variants `Loaded(LoadStats)/Reused/Skipped/Failed`; `LoadStats` is from
`cih-graph-store`, ungated) is referenced unconditionally by `print_styled`/`summary`.
- Move `LoadOutcome` + its `impl` to an **ungated** location — new
  `crates/cih-engine/src/load_outcome.rs` (or into `analyze/mod.rs`).
- Keep `load_to_falkor`, `load_many_to_falkor`, `load_to_falkor_with_progress`,
  `PhaseObserver`, and all `FalkorStore` use in `db.rs`.
- `lib.rs`: `#[cfg(feature = "falkor")] pub mod db;` and `pub mod load_outcome;`
  (ungated). Re-export `LoadOutcome` from wherever consumers import it today.

### Step 4 — Gate the FalkorDB load branch in `analyze`
In `run_analyze` (`analyze/mod.rs`), replace the single `let load = …` (which calls
`load_to_falkor_with_progress` in its `else` arm) with cfg'd definitions so the
FalkorDB path and `DEFAULT_FALKOR_URL`/`graph_key` are absent when `falkor` is off:
```rust
#[cfg(feature = "falkor")]
let load = { /* existing: reused → Reused; no_load||artifacts_only → Skipped;
               else → load_to_falkor_with_progress(...) */ };
#[cfg(not(feature = "falkor"))]
let load = LoadOutcome::Skipped;
```
- Gate the `use crate::db::load_to_falkor_with_progress;` import behind `falkor`.
- Gate the `matches!(load, LoadOutcome::Failed(_)) { process::exit(3) }` on `falkor`
  (no DB, no exit 3).
- **Also gate `run_resolve` itself** (`analyze/mod.rs:128`) with
  `#[cfg(feature = "falkor")]` — it calls `load_to_falkor_with_progress`
  unconditionally and lives in this ungated module; gating only the `Resolve` clap
  variant is not enough.
- The `let graph_key = …DEFAULT_GRAPH_KEY` used by `registry::persist_analyze`
  (`analyze/mod.rs:119`) is *outside* the load branch and stays ungated.
- `print_styled`: when the FalkorDB row would show `Skipped`, render it as
  `not built` under `#[cfg(not(feature = "falkor"))]` (or omit the row) — reuse the
  existing `LoadOutcome::Skipped` arm.

### Step 5 — `--artifacts-only` flag (full build convenience)
Add `--artifacts-only` to `AnalyzeArgs` (`cmd/args.rs`); thread into `AnalyzeFlags`.
Treat `flags.no_load || flags.artifacts_only` as "skip load" in the `falkor` arm of
Step 4. (`--no-load` stays as-is.)

### Step 6 — Zero-config: implicit `--all`
In `run_analyze` at the no-selector check (`analyze/mod.rs:63-68`), replace the
`print_summary` + "Choose a scope" + `process::exit(2)` with a fallback to the same
`ScopeRequest` that `--all` builds (e.g. rebuild via `build_scope_request` with
`all = true`, or construct the all-scope request directly). `--module`, `--include`,
and `cih.scope.toml` still override; cwd-default is already in place.

### Step 7 — Gate DB/embed-only code (per-item, not blanket)
clap derive honors `#[cfg]` on enum variants, so a gated command simply doesn't
exist when the feature is off. The precise gating per item:

**Gated wholesale behind `falkor`** (inherently DB-bound; restored in M2):
- `Resolve` — variant (`cmd/args.rs`), dispatch arm (`cmd/mod.rs`), and
  `run_resolve` (Step 4).
- `Discover` — variant, dispatch arm, `cmd/discover.rs`, and
  `#[cfg(feature = "falkor")] pub mod discover;` in `lib.rs` (it imports
  `load_many_to_falkor` at `discover.rs:57`).
- `Refresh` — variant, dispatch arm, `cmd/refresh.rs` (calls `run_discover`,
  `refresh.rs:12,155`). **Previously missed — the build fails without this.**
- `ArtifactCommand::Bootstrap` — the *only* artifact arm using `FalkorStore`
  (`cmd/artifact.rs:55-80`). `Export`/`Import` are pure artifact ops and stay in
  the standalone build.

**Kept in standalone, only the load step gated behind `falkor`:**
- `Taint` — its falkor use is exactly the final optional load block
  (`cmd/taint.rs:262-291`), the same shape as analyze's. Apply the same cfg
  treatment as Step 4: `#[cfg(feature = "falkor")]` load arm,
  `#[cfg(not(...))] let load = LoadOutcome::Skipped;`. Offline taint analysis
  (artifacts + report) keeps working — `cih-taint` itself has no falkor dep.

**Gated behind `embedding`:**
- `Embed` — variant, dispatch arm, and `pub(crate) mod embed;` in `lib.rs`
  (`embed.rs` uses only `cih_embed`).
- Inside `discover.rs` (which is already `falkor`-gated): the embed-strategy
  path (`cih_embed::embeddable_nodes` / `EmbedStore::connect`,
  `discover.rs:551-592`) gets an additional `embedding` gate with a runtime
  error fallback, so `--features falkor` alone compiles (design decision 5).
- In `feature_strategy.rs`: **gate only** the `use cih_embed::…` import and the
  `Embed` arm of `build_feature_strategy` — never the module, because the
  ungated `Features` command uses `make_feature_llm_caller`
  (`cmd/features.rs:568`).

**Stay core in every build (verified — zero falkor/embed references):** `Scan`,
`Analyze`, `Status`, `List`, `Config`, `Wiki`, `Group`, `Features`, `Ui`, `Start`,
and `group_sync`.

## Files to modify
- `crates/cih-engine/Cargo.toml` — features, optional deps, `cih` bin target.
- `crates/cih-engine/src/lib.rs` — `#[cfg]` `mod db`; add ungated `mod load_outcome`.
- `crates/cih-engine/src/load_outcome.rs` — **new**, holds `LoadOutcome`.
- `crates/cih-engine/src/db.rs` — drop `LoadOutcome` (moved); rest unchanged.
- `crates/cih-engine/src/analyze/mod.rs` — cfg load branch **and `run_resolve`**;
  implicit `--all`; summary.
- `crates/cih-engine/src/cmd/args.rs` — `--artifacts-only`; cfg gated command
  variants (`Resolve`, `Discover`, `Refresh`, `Embed`, `ArtifactCommand::Bootstrap`).
- `crates/cih-engine/src/cmd/mod.rs` — cfg gated dispatch arms.
- `crates/cih-engine/src/cmd/{discover.rs,refresh.rs}` — falkor-gated modules.
- `crates/cih-engine/src/discover.rs` — falkor-gated module; embed path
  additionally `embedding`-gated with runtime fallback.
- `crates/cih-engine/src/cmd/taint.rs` — cfg only the FalkorDB load block.
- `crates/cih-engine/src/cmd/artifact.rs` — cfg only the `Bootstrap` arm.
- `crates/cih-engine/src/{feature_strategy.rs,embed.rs}` — embedding gates
  (import + `Embed` strategy arm in the former; whole module for the latter).
- `crates/cih-engine/src/bin/cih.rs` — **new** shim.

## Verification

1. **Dev build unchanged:** `cargo build` and `cargo test --workspace` (default
   features) green; `cih-engine analyze <repo> --all` still loads FalkorDB as today.
2. **Standalone core compiles clean:**
   `cargo build -p cih-engine --no-default-features` and
   `cargo clippy -p cih-engine --no-default-features --all-targets -- -D warnings`;
   also `cargo test -p cih-engine --no-default-features` (tests import only
   ungated modules, so this should pass without test-file gating).
   **Feature-matrix check** (design decision 5):
   `cargo check -p cih-engine --no-default-features --features falkor` and
   `… --features embedding` both compile.
3. **Tree is free of the heavy deps:**
   `cargo tree -p cih-engine --no-default-features -e no-dev` shows **none** of
   `ort, ort-sys, fastembed, tokio-postgres, pgvector, redis, cih-falkor, cih-embed`.
   When a GNU toolchain is available, repeat with `--target x86_64-pc-windows-gnu`.
4. **Offline analyze (end-to-end):** with the no-default-features binary, on a copy
   of `crates/cih-engine/tests/corpus/js-cjs-express` in the job tmp dir and **no DB
   running**: `cih analyze <copy>` (no `--all`) writes
   `.cih/artifacts/<version>/{nodes,edges}.jsonl`, prints the summary with the
   FalkorDB row absent/`not built`, and exits 0.
5. **Full-build opt-out:** `cih analyze <repo> --artifacts-only` (default features)
   skips the DB and exits 0.
6. **Formatting:** `cargo fmt --all --check`.

## Risks

- **Whole-tree Windows-GNU build is the real unknown** — tree-sitter grammars
  (compiled via `cc`), zstd, blake3, tokio. Rust refactors here are low-risk; proving
  the GNU build in CI is the gating step. Step 3's tree check is the go/no-go signal.
- **`cih-server` still needs FalkorDB** (its own crate depends on `cih-falkor`
  unconditionally). M1 does not make `serve` offline — only the engine's `analyze`.
- **Gated subcommands disappear** from the standalone surface until M2
  (`resolve`, `discover`, `refresh`, `embed`, `artifact bootstrap`); acceptable
  and intended. `taint` and `artifact export/import` remain available offline.
- **Mixed feature combos are easy to regress** — without CI checks for
  `--features falkor` alone and `--features embedding` alone, a later change can
  silently break additivity. The feature-matrix `cargo check` in Verification
  should go into CI alongside the default build.
- **Graph-store decoupling is already complete.** M1's `falkor` feature should
  forward to `cih-store-factory/falkor`, concentrating the cfg gates in the
  factory crate instead of engine internals.
