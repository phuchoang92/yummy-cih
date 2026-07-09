# cih-engine: Architecture Review → CLI-Layer & File-Organization Cleanup

## Context

Phuc asked for an architecture + file-organization review of `cih-engine` (~17k LOC, 59 files) with cleanup. Two thorough explorations produced a clear verdict:

**The architecture is healthy.** No dependency cycles; every suspected overlap with sibling crates is deliberate layering, not duplication (`registry.rs` adapts `cih_core::Registry`; `db.rs` orchestrates `cih-falkor` staging/publish; `feature_strategy.rs` is a builder over `cih-grouping` traits; `llm/grouping.rs` [wiki module tree] vs `cih-grouping/strategies/llm.rs` [flat feature classification] solve different problems). The engine-wiki/cih-wiki boundary is clean (enrichment vs rendering). Subsystem internals (analyze/, scan/, llm/, wiki/) are well-factored — big files there are coherent orchestrators, not god-objects.

**The problems are organizational, concentrated in the CLI layer:**
1. `main.rs` is 1,374 lines: ~370 of clap arg structs + fat dispatch arms holding real logic — settings resolution for Analyze (~69), Discover (~148), Wiki (~152), plus List (~23), Status (~72), Artifact (~91) bodies. That logic is **untestable where it sits**.
2. 18 root modules mix four roles; `cmd/` holds only 4 of ~11 command implementations (`start.rs` 790L, `tui.rs` 795L, `group_sync.rs` 360L sit at the root).
3. `lib.rs` pub-mods all 24 modules — no curation, no doc header.
4. One dead field: `EmitOutcome::unresolved_report_path` (`analyze/mod.rs:679`) — set, never read.

Scope confirmed with Phuc: **CLI layer + commands into `cmd/`** (not the full phases/utilities regroup), and **settings resolution becomes testable functions in `settings.rs`**.

**Prerequisite:** this builds on the `naming-cleanup` branch (PR #3 — lib target renamed to `cih_engine`). Merge PR #3 first, branch `engine-cli-cleanup` off `dev`; if #3 isn't merged yet, branch off `naming-cleanup`.

## Target layout

```
crates/cih-engine/src/
├── main.rs            # ~10-line shim: cih_engine::cmd::main()
├── lib.rs             # doc header + curated exports (see Phase D)
├── cmd/               # the COMPLETE CLI layer — one file per command (family)
│   ├── mod.rs         # pub fn main(): Cli::parse() + dispatch (every arm thin)
│   ├── args.rs        # Cli, Command, DbArgs, ArtifactCommand, ConfigCommand,
│   │                  #   GroupCommand, FeaturesCommand (moved verbatim from main.rs)
│   ├── analyze.rs     # NEW  thin: settings::resolve_analyze + analyze::run_analyze
│   ├── discover.rs    # NEW  thin: settings::resolve_discover + discover::run_discover
│   ├── wiki.rs        # NEW  thin: settings::resolve_wiki + wiki::run_wiki
│   ├── artifact.rs    # NEW  from run_artifact() helper (Export/Import/Bootstrap)
│   ├── list.rs        # NEW  from List arm
│   ├── status.rs      # NEW  from Status arm
│   ├── config.rs      # existing
│   ├── features.rs    # existing
│   ├── group.rs       # existing
│   ├── group_sync.rs  # MOVED from src/group_sync.rs
│   ├── start.rs       # MOVED from src/start.rs
│   ├── start_env.rs   # MOVED from src/start_env.rs
│   ├── taint.rs       # existing
│   └── tui.rs         # MOVED from src/tui.rs
├── analyze/ scan/ llm/ wiki/        # unchanged (pipeline subsystems)
├── discover.rs embed.rs decompile.rs decompile_config.rs   # pipeline phases, unchanged
└── db.rs feature_strategy.rs file_cache.rs node_prefix.rs
    registry.rs runtime.rs scope.rs settings.rs ui.rs versioning.rs  # utilities, unchanged
```

## Phases (one commit each)

### Phase A — Testable settings resolution (`src/settings.rs`)

Add three functions that replicate the fat-arm logic **exactly** (behavior-preserving):

```rust
pub fn resolve_analyze(flags: &AnalyzeFlagInputs, layers: &Layers) -> AnalyzeSettings
pub fn resolve_discover(flags: &DiscoverFlagInputs, layers: &Layers) -> DiscoverSettings  // incl. LLM config builder
pub fn resolve_wiki(flags: &WikiFlagInputs, layers: &Layers) -> WikiSettings
```

- Reuse the existing `Resolved<T>` provenance type, `resolve()`/`resolve_bool()` helpers, and the `AnalyzeSettings`/`DiscoverSettings`/`WikiSettings` structs already in `settings.rs` (`crates/cih-engine/src/settings.rs:70-225`).
- Flag-input structs mirror the clap fields the arms read today (main.rs Analyze arm ~lines 590-660, Discover ~660-810, Wiki ~880-1030 — copy the precedence logic verbatim).
- **Unit tests** in `settings.rs`: temp repo with a `cih.toml`, temp home config, assert flag > env > repo > home > default precedence for representative fields of each command (community_strategy, languages, llm_provider, etc.).

### Phase B — Extract the CLI layer into `cmd/`

- `cmd/args.rs`: move `Cli`, `Command`, `DbArgs`, `ArtifactCommand`, `ConfigCommand`, `GroupCommand`, `FeaturesCommand` from main.rs verbatim (clap derives untouched → `--help` output must not change).
- New thin command files (`cmd/analyze.rs`, `discover.rs`, `wiki.rs`): call the Phase-A resolvers, then the existing phase entry points (`analyze::run_analyze`, `discover::run_discover`, `wiki::run_wiki`).
- `cmd/artifact.rs`, `cmd/list.rs`, `cmd/status.rs`: move the arm/helper bodies as-is (they're I/O + formatting, not settings logic).
- `cmd/mod.rs`: `pub fn main() -> anyhow::Result<()>` = `Cli::parse()` + a match where **every** arm is one call.
- `main.rs` becomes the shim (keep the crate doc comment; move the FALKOR/GRAPH_KEY notes if referenced).

### Phase C — Complete the command family (moves)

- `git mv src/start.rs src/cmd/start.rs`, `src/start_env.rs → src/cmd/start_env.rs`, `src/tui.rs → src/cmd/tui.rs`, `src/group_sync.rs → src/cmd/group_sync.rs` (history-preserving).
- Fix `crate::` paths inside the moved files and their four integration tests (`tests/start.rs`, `tests/start_env.rs`, `tests/group_sync.rs`, plus `crate_tests.rs` if it touches them): `cih_engine::start::…` → `cih_engine::cmd::start::…`.
- `cmd/group.rs` keeps dispatching into `cmd/group_sync.rs`.

### Phase D — lib.rs façade + small kills

- `lib.rs`: crate-level doc comment stating the organization (cmd = CLI layer; analyze/scan/discover/embed/decompile = pipeline phases; wiki+llm = enrichment; the rest = shared utilities).
- Demote modules to `pub(crate)` that neither tests nor external consumers import — after Phase B the binary only calls `cmd::main()`, so candidates are `node_prefix`, `registry`, `runtime`, `ui`, `feature_strategy`, `decompile_config`, `embed`, `decompile` (let the compiler + test suite decide the exact set; keep pub whatever `tests/` imports: llm, scan, wiki, analyze, scope, db, discover, file_cache, settings, cmd).
- Remove the dead `EmitOutcome::unresolved_report_path` field (`analyze/mod.rs:679`, set at lines ~335/~581).
- Update CLAUDE.md's engine layout description if it names moved files.

## Verification

- Per phase: `cargo test --workspace` (hermetic) and `cargo clippy --workspace --all-targets -- -D warnings` (the CI gate is blocking now — it will catch any missed `pub`/path fallout).
- **CLI surface unchanged:** capture `cih-engine --help` and each subcommand's `--help` before Phase B and diff after — must be byte-identical (arg structs moved verbatim).
- **Settings behavior unchanged:** before Phase A, run `cargo run --release -p cih-engine -- analyze <fixture> --no-cache --no-load` on `cih-eval-repos/servicemix` (smaller than fineract) and record the artifact hash + `cih-engine config show` output; repeat after Phase D — resolved settings and artifacts must match.
- Phase-A unit tests prove the precedence chain for each new resolver.
- Final smoke: `cih-engine ui` (tui launches), `cih-engine start --help`, `cih-engine group list` — the moved commands still dispatch.

## Out of scope (deliberate)

- Grouping phases under `pipeline/` or utilities under `support/` (declined — churn > value).
- Splitting `analyze/mod.rs` (794L) or `wiki/run.rs` (582L) — exploration verdict: coherent orchestrators; size alone isn't a trigger.
- Moving `llm/evidence.rs`/`llm/grouping.rs` into `wiki/` — they're wiki-only today, but keeping all LLM machinery in `llm/` is the clearer boundary.
- The `cih-resolve` index/emit dedup — separate plan already at `docs/plans/resolve-index-dedup.md`.

## Effort

Phase A is the careful one (replicating ~370 lines of precedence logic into testable functions). B–D are mechanical with the compiler driving. Copy this plan to `docs/plans/engine-cli-cleanup.md` as step 0 (repo convention).
