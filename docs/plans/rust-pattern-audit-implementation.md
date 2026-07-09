# yummy-cih: Implement the Rust Pattern-Audit Recommendations

## Context

An audit of the yummy-cih Rust workspace against ecosystem patterns (`/Users/phuc/BigMoves/AI/yummy-cih-rust-pattern-audit.md`) found the macro-architecture excellent but flagged six mid-level improvements. Phuc approved implementing **all of them**, with two decisions made during planning:

- **Keep the browser UI** (`browser.rs`/`layout.rs`) — exploration proved it is a working, tested graph browser served at `/graph`, not dead code. The stale "superseded/dead" comments in CI and CLAUDE.md get corrected instead.
- **All optional items in scope**: strum derives, symbol-ID interning, and ratcheting clippy `-D warnings` to the full workspace.

Exploration also corrected the audit's sizing:
- `NodeId` is already 99% encapsulated (1 direct `.0` access workspace-wide; everything else uses `NodeId::new()`). **`VersionId` is the real offender**: no methods, 29 `.0` reads + 8 raw `VersionId(...)` constructions.
- `cih-resolve` needs almost no error work — its core (`resolve_edges`/`resolve_with_registry`) is *infallible by design* (diagnostics via `UnresolvedRef` records) and `reports.rs` already returns `io::Result`. Only `cih-parse` gets a `ParseError` enum.
- serde is a non-issue for privatizing fields (derives access private fields in the defining crate; newtype serializes transparently either way).

Repo: `/Users/phuc/BigMoves/AI/yummy-cih` (not currently on a branch check — **branch first**, per CLAUDE.md "don't commit on the default branch"). Convention: copy this plan into `docs/plans/rust-pattern-audit-implementation.md` as step 0.

## Workstreams (in execution order)

Each workstream is independently verifiable (`cargo test --workspace` is hermetic — no FalkorDB/Postgres needed) and should be a separate commit.

### WS0 — Setup
- Create branch (e.g. `pattern-audit-cleanup`).
- Copy this plan to `docs/plans/rust-pattern-audit-implementation.md`.

### WS1 — cih-core type hardening (NodeId, VersionId, strum)

**Files:** `crates/cih-core/src/lib.rs` (+ ~40 call sites below).

1. `NodeId`: make field private (`pub struct NodeId(String)`). Keep `new`, `as_str`, `Display`. Fix the single external access: `crates/cih-server/src/layout.rs:150` `item.node.id.0` → `item.node.id.to_string()` (it moves into an owned field) or `.as_str().to_owned()`.
2. `VersionId` (`crates/cih-core/src/lib.rs:346`): add `impl VersionId { pub fn new(impl Into<String>), pub fn as_str(&self) -> &str }` + `Display`, then privatize the field. Fix sites:
   - 29 `.0` reads — cih-engine (24: `discover.rs`, `embed.rs`, `cmd/taint.rs`, `wiki/loader.rs`, `analyze/cache.rs`), cih-core (2: `artifacts.rs:83,218`), cih-server (3: `search.rs:88`, `browser.rs:317`, `tests/search.rs:56`). Pattern: `%v.0` → `%v` in tracing; `.0.cmp(&x.0)` → `.as_str().cmp(x.as_str())`; `.0.clone()` → `.clone()` where the whole VersionId is wanted.
   - 8 raw constructions (`cih-engine` ×5, `cih-server/src/utils.rs` ×2, `cih-falkor/examples` ×1) → `VersionId::new(...)`.
3. **strum**: add `strum = { version = "0.27", features = ["derive"] }` to `[workspace.dependencies]` + cih-core. Replace the hand-rolled `NodeKind::label()`/`from_label()` 20-arm matches (`lib.rs:82-135`) with `strum::IntoStaticStr` + `EnumString` derives; keep `label()`/`from_label()` as thin shims delegating to strum so no call sites change. Apply the same to `EdgeKind` (`impl` at `lib.rs:256`) if it has the same mapping shape. **Constraint:** emitted label strings must stay byte-identical (they're graph labels in FalkorDB) — the existing cih-core unit tests plus a new round-trip test (`for k in all kinds: from_label(label(k)) == k`) guard this.

### WS2 — ParseError enum in cih-parse; document anyhow-by-design elsewhere

**Files:** `crates/cih-parse/src/lib.rs`; crate docs of `cih-resolve`, `cih-lang`, `cih-wiki`.

1. Add a `ParseError` enum mirroring the house style of `GraphStoreError` (`crates/cih-graph-store/src/lib.rs:14-26`) and `TaintError` (`crates/cih-taint/src/error.rs`):
   ```rust
   #[derive(Debug, thiserror::Error)]
   pub enum ParseError {
       #[error("no language provider for {0}")]
       NoLanguageProvider(String),
       #[error("failed to read {path}")]
       Read { path: PathBuf, #[source] source: std::io::Error },
       #[error("parse failed for {path}")]
       Parse { path: String, #[source] source: anyhow::Error }, // provider errors are anyhow today
       #[error("artifact I/O at {path}")]
       ArtifactIo { path: PathBuf, #[source] source: std::io::Error },
       #[error("artifact JSON at {path}:{line}")]
       ArtifactJson { path: PathBuf, line: usize, #[source] source: serde_json::Error },
   }
   pub type Result<T> = std::result::Result<T, ParseError>;
   ```
   Convert the ~9 error sites (`parse_one`, `write_parsed_files`, `load_parsed_files`). **Keep the per-file collection semantics untouched** — `parse_file_units` still folds per-file errors into `SkippedFile { reason }` (now `err.to_string()` walks the source chain via `{err:#}`-equivalent formatting; preserve current reason text quality).
   Engine call sites (`analyze/cache.rs:59,179,251`, `analyze/mod.rs:521`) keep working via `?` because `anyhow::Error: From<ParseError>` automatically. The silent `unwrap_or(0)` at `cache.rs:251` is intentional (cache miss → reparse) — leave it, add a comment.
2. `cih-resolve`: no enum. Add crate-level doc (`lib.rs` header) stating the design: core resolve is infallible, diagnostics via `UnresolvedRef`, report I/O is `io::Result`. Same one-paragraph "errors are anyhow by design (leaf orchestration)" note in `cih-lang` and `cih-wiki` crate docs.

### WS3 — FxHashMap/FxHashSet for NodeId-keyed maps

**Files:** ~58 type declarations across 5 crates; `rustc-hash` is already in `[workspace.dependencies]`.

- Add `rustc-hash.workspace = true` to: cih-taint, cih-core, cih-resolve, cih-search, cih-engine (cih-community already has it).
- Swap `HashMap<NodeId|&NodeId, _>` → `FxHashMap` and `HashSet<NodeId|&NodeId>` → `FxHashSet`:
  cih-taint (25 sites: `pdg.rs`, `analyzer.rs`, `interproc.rs`, `liveness.rs`, `flow_sensitive.rs`), cih-community (26: `graph.rs`, `bfs.rs`, `lib.rs`), cih-core (6: `entrypoints.rs`), cih-resolve (6: `db_access.rs`, `lang/java/cxf.rs`), cih-search (2: `rrf.rs`), cih-engine (2: `cmd/taint.rs`).
- Mechanical: `FxHashMap::default()` replaces `HashMap::new()`; `.collect()` works unchanged. Don't touch String-keyed or other maps in this pass.

### WS4 — cih-server: kill lib/bin duplication, curated façade, fix stale comments

**Files:** `crates/cih-server/src/main.rs` (762 lines), `src/lib.rs`, CI + CLAUDE.md comment fixes.

1. **Dedup:** `main.rs` currently redeclares `mod` for all 20 modules (compiled twice — once for bin, once for lib `cih_server`). Move main.rs's substance (server wiring, MCP tool definitions, axum router assembly incl. `browser::router` merge at `main.rs:733`) into the lib — e.g. new `src/tools.rs` + `pub fn run(...)` in a `src/app.rs` — and shrink `main.rs` to a shim: parse args/env, call `cih_server::run()`.
2. **Façade:** rewrite `lib.rs` — keep `pub mod` only for what external consumers/tests use (`args`, `browser`, `viz`, `utils`, `search`, plus newly promoted `patterns` and the `run` entry point); demote the rest (`agent`, `changes`, `config`, `contracts`, `coverage`, `feature`, `files`, `indexing`, `jobs`, `layout`, `resources`, `server`, `symbol`, `taint`) to `pub(crate) mod`. Tests in `tests/{browser,viz,args,search}.rs` define the stable surface — don't break their imports.
3. **Browser UI stays.** Update the stale comments: `.github/workflows/ci.yml:48-51` ("superseded browser/layout UI code") and the CLAUDE.md "dead UI code" line → describe it as the local graph-browser UI, kept.

### WS5 — Clippy ratchet to full workspace

**Files:** `.github/workflows/ci.yml` (lines ~39–51), warning fixes across crates.

- Iterate in dependency order (lang → parse → resolve → community → search → embed → jar → patterns → grouping → wiki → engine → server): run `cargo clippy --all-targets -p <crate> -- -D warnings`, fix findings, move the crate from the non-blocking job into the gated `-p` list. cih-server goes in right after WS4.
- End state: gated job becomes `cargo clippy --workspace --all-targets -- -D warnings`; delete the non-blocking job and its TODO comments.
- Fix warnings minimally (prefer real fixes; `#[allow]` with a comment only where a lint is wrong for the code). `cargo fmt` remains explicitly out of scope (separate documented TODO).
- Expect the long tail in cih-engine (TUI, 20k LOC) — budget the most time here; it's fine to gate crates incrementally across multiple commits.

### WS6 — Symbol-ID interning in the taint hot path (measured)

**Outcome (2026-07-09): rejected by the measurement gate.** Baseline on Fineract
(~46k nodes, artifacts pre-built): full 4-phase `taint --no-load` = **0.75–0.77s
wall (3 runs)**. There is no hot path to optimize; the FxHashMap swap (WS3) already
covers the cheap win. No interner was added (an unused utility would contradict the
audit's build-for-need principle); negative result recorded in docs/ARCHITECTURE.md.

Original plan (kept for reference) — the largest, riskiest item, only landed if it measures.

1. **Baseline first:** time `cargo run --release -p cih-engine -- analyze <fixture>` on a real repo (e.g. one of `~/BigMoves/AI/cih-eval-repos`, or the Fineract checkout) 3× with hyperfine or `time`; record parse/resolve/taint phase timings (the engine logs phases via tracing).
2. Add a small hand-rolled interner to cih-core (`src/intern.rs`, ~60 lines, no new dep): `Interner { map: FxHashMap<Box<str>, Sym>, strings: Vec<Box<str>> }`, `Sym(u32)`, `intern(&str) -> Sym`, `resolve(Sym) -> &str`; unit tests.
3. Apply where NodeId-keyed maps sit in inner loops — **cih-taint first** (`pdg.rs` `by_target`/`by_source`, `flow_sensitive.rs` `tainted_defs`, `interproc.rs` visited sets): intern NodeIds once when the PDG is built, run the dataflow on `Sym` keys, resolve back to NodeId only in reported findings. cih-community already interns effectively (NodeId → petgraph `NodeIndex` in `graph.rs:15`) — leave it.
4. Re-measure. Land if taint/analyze phase improves measurably (>5%); otherwise revert step 3, keep the interner + a note in `docs/ARCHITECTURE.md` recording the negative result.

## Out of scope (deliberate)

- **async-trait → native async in traits**: `GraphStore` is consumed as `Arc<dyn GraphStore>`; native `async fn` in traits is not dyn-compatible, so `async_trait` stays. (The audit's "on next edition bump" note is superseded by this finding.)
- `cargo fmt` pass, layering enforcement via cargo-deny, resolve-crate error enums.

## Verification

- After every workstream: `cargo test --workspace` (hermetic, no services needed) + `cargo clippy --all-targets -p <touched crates> -- -D warnings`.
- WS1: new NodeKind/EdgeKind label round-trip test passes; grep confirms zero remaining `\.0` on NodeId/VersionId outside cih-core.
- WS4: `cargo build -p cih-server` compiles each module once (build succeeds with the shim main); `cargo test -p cih-server` including `tests/browser.rs`; smoke: `cargo run -p cih-server` with `FALKOR_URL=redis://127.0.0.1:6380` + `CIH_ALLOW_INSECURE=1`, then hit `/graph` and an MCP tool (e.g. `list_repos`) to confirm the browser UI and tools still serve.
- WS5: CI-equivalent locally: `cargo clippy --workspace --all-targets -- -D warnings` clean.
- WS6: benchmark numbers recorded before/after in the plan doc under `docs/plans/`.
- End-to-end (optional but recommended once at the end): `cih-engine analyze` a small fixture repo, load it, run `context`/`impact` via the MCP server to confirm graph labels unchanged (guards the strum swap).
