# Graph-store decoupling — make the graph DB pluggable

## Goal

Any graph database plugs into CIH by (a) implementing the `GraphStore` trait,
(b) registering one arm in a shared backend factory, and (c) passing a
backend-neutral contract test suite. FalkorDB becomes *an* adapter instead of
*the* database.

This is the enabling refactor for the standalone roadmap's M2 `LocalGraphStore`
(see `docs/plans/standalone-milestone-1-offline-analyze.md` for M1): once this
lands, a new backend is one new crate + one factory arm + a green contract run.

## Background (verified against the code)

The ports-and-adapters seam already exists — `GraphStore`
(`crates/cih-graph-store/src/lib.rs`, ~25 read methods + `ensure_schema` /
`bulk_load` / `upsert_incremental` / `publish_to`) with the `cih-falkor`
adapter — and the entire MCP/server layer runs on `Arc<dyn GraphStore>`. FalkorDB
is nevertheless not swappable, because exactly four places reach past the port
to the concrete type:

1. **`cih-engine/src/db.rs:5,52,152`** — the analyze/discover load path calls
   `FalkorStore::connect` directly and uses two **inherent-only** methods that
   don't exist on the trait: `drop_graph` and `bulk_load_observed` (the
   phase-progress entry point).
2. **`cih-engine/src/cmd/artifact.rs:74-76`** — `artifact bootstrap` connects
   concretely (everything after the connect is already trait calls).
3. **`cih-server/src/config.rs:147-185`** — `build_store` is the right factory
   pattern, but `falkor` is the only implemented arm (`neptune`/`postgres` are
   stub errors).
4. **`cih-server/src/app.rs:167-181`** — `store_for` (per-graph-key stores for
   multi-repo groups) bypasses the factory with a hard-coded
   `FalkorStore::connect`. It does apply `with_query_limit` (via the
   `store_limits` field), so tuning is already consistent with startup. The
   one behavioral difference vs `build_store` is `ensure_schema`: startup
   retries it 5× with backoff (riding out DB boot races), `store_for` calls
   it once (failing fast while serving traffic is correct). That difference
   is deliberate and stays — schema init remains caller policy; the factory
   only constructs.

Falkor-specific internals that correctly stay inside the adapter:
`wait_until_ready`/`BusyLoading` handling, `GRAPH.BULK` encoding, the connect
timeout, `is_loading_error`, `with_query_limit` (construction-time tuning).

## Design decisions

1. **Extend the port, don't invent a parallel one.** `drop_graph` and
   `bulk_load_observed` become `GraphStore` trait methods. `bulk_load_observed`
   gets a **default implementation** that ignores the observer and delegates to
   `bulk_load`, so adapters without phase events implement nothing extra.
2. **One factory crate, feature-gated.** `cih-graph-store` cannot depend on
   adapters (dependency direction), and factory logic is currently duplicated
   between server (`config.rs`) and engine (hard-coded). A new tiny
   `cih-store-factory` crate is the single place adapter deps live behind Cargo
   features and `CIH_GRAPH_BACKEND` names are parsed. The standalone M1 `falkor`
   feature on `cih-engine` forwards to this crate's feature.
3. **Staging/publish stays engine policy, defined per-adapter — with an
   explicit safety contract.** The engine's flow (connect to `{key}-staging` →
   `drop_graph` → `bulk_load_observed` per set → `publish_to(key)` →
   `drop_graph` on staging) is already the right generic shape. Adapters
   define what publish means: Falkor = Redis RENAME (O(1), atomic); a local
   store = atomic file/dir swap. **Port guarantee (contract-tested):** after
   `publish_to(dest)` returns, dropping the source (staging) graph must not
   affect the published data. Falkor satisfies this only by accident today —
   RENAME removes the staging key, so the trailing `drop_graph` fails and is
   warn-swallowed — and a backend that "published" by aliasing, or by treating
   staging as the live key with a no-op `publish_to`, would have its live
   graph destroyed by that drop. So: no no-op publishes that share storage
   with the staging key; a backend without cheap rename must copy on publish.
   (If a future backend truly can't afford that, the extension is an
   adapter-provided staging strategy — e.g. `staging_key_for(key) ->
   Option<String>`, where `None` means load in place and the engine skips
   publish+drop — never a silent no-op.)
4. **No flag/env renames now.** Add `--backend` (env `CIH_GRAPH_BACKEND`,
   default `falkor`); keep `--falkor-url`/`FALKOR_URL` untouched. Renaming to
   `--db-url`/`CIH_DB_URL` is cosmetic churn deferred to the standalone M4
   packaging pass (with aliases).
5. **Pluggability is enforced by a contract suite, not by documentation.** The
   Falkor behavior tests get ported into a generic suite parameterized over a
   store constructor; every adapter (current and future) runs the same suite.

## Implementation steps

### Step 1 — Port extension (`cih-graph-store/src/lib.rs`)
```rust
#[async_trait]
pub trait GraphStore: Send + Sync {
    // ... existing methods ...
    async fn drop_graph(&self) -> Result<()>;
    /// Bulk load with phase callbacks. Default ignores the observer so adapters
    /// without phase events need no extra code; Falkor overrides with its
    /// staged GRAPH.BULK implementation.
    async fn bulk_load_observed(
        &self,
        artifacts: &GraphArtifacts,
        obs: &dyn LoadObserver,
    ) -> Result<LoadStats> {
        let _ = obs;
        self.bulk_load(artifacts).await
    }
}
```
`cih-falkor`: move the inherent `drop_graph` (`lib.rs:180`) and
`bulk_load_observed` (`lib.rs:390`) into the `GraphStore` impl in `query.rs` —
bodies unchanged. The trait `bulk_load` keeps delegating to
`bulk_load_observed(&NoopObserver)` (no recursion risk: `bulk_load` stays a
required method, so the mutual delegation can't loop).

Also delete the dead seam this promotion obsoletes: the `BulkLoader` trait
(`cih-graph-store/src/lib.rs:306`) and `FalkorBulkLoader`
(`cih-falkor/src/lib.rs:539`) have no consumers outside their defining crates —
its "the engine depends on the `BulkLoader` trait" comment is aspirational, not
true — and keeping it would leave two competing load abstractions. Reword the
doc-comment mentions of `BulkLoader` in `cih-core` (`lib.rs:339`,
`artifacts.rs:3`) to refer to `GraphStore::bulk_load`.

### Step 2 — `crates/cih-store-factory` (new)
```toml
[package] name = "cih-store-factory"
[dependencies]
cih-graph-store.workspace = true
cih-falkor = { workspace = true, optional = true }
anyhow.workspace = true
[features]
default = ["falkor"]
falkor = ["dep:cih-falkor"]
```
```rust
pub struct StoreOptions {
    /// (max_concurrent, acquire_timeout) — server backpressure; None for CLI use.
    pub query_limit: Option<(usize, std::time::Duration)>,
}

/// `backend`: "falkor" | "local" (M2) | "neptune" | "postgres" (stubs).
/// Unknown/unbuilt backends error with the list of compiled-in ones.
pub fn connect_store(
    backend: &str, url: &str, graph_key: &str, opts: &StoreOptions,
) -> anyhow::Result<std::sync::Arc<dyn cih_graph_store::GraphStore>>;
```
The `falkor` arm applies `with_query_limit` when `opts.query_limit` is set. Add
the crate to the workspace `members`.

### Step 3 — Engine load path over the trait (`cih-engine/src/db.rs`)
Rework `load_many_to_falkor` / `load_to_falkor_with_progress` into
backend-generic `load_many` / `load_with_progress(backend, url, graph_key,
artifacts, quiet)` built on `connect_store` + trait calls only. Keep the
existing fn names as thin wrappers passing the resolved backend (so
`analyze/mod.rs`, `discover.rs`, `cmd/taint.rs` diffs stay minimal).
`PhaseObserver`, `LoadOutcome`, and the five-phase UI are untouched.
- `cmd/args.rs`: add `--backend` to the shared `DbArgs` group
  (`env = "CIH_GRAPH_BACKEND"`, default `falkor`); thread through
  `AnalyzeFlags` and the discover/taint flag structs alongside `falkor_url`.

### Step 4 — Remaining concrete sites
- `cmd/artifact.rs` Bootstrap arm: replace `FalkorStore::connect` with
  `connect_store(...)`; the following `ensure_schema` + `bulk_load` calls are
  already trait methods.
- `cih-server/src/config.rs::build_store`: body becomes
  `connect_store(&cfg.backend, &cfg.falkor_url, &cfg.graph_key, &opts)` plus the
  existing backend-agnostic `ensure_schema` retry loop (already pure trait
  calls). The `neptune`/`postgres` stub messages move into the factory.
- `cih-server/src/app.rs::store_for`: route through `connect_store` with the
  same `StoreOptions` as startup (its single-shot `ensure_schema` stays, per
  Design decision on schema init being caller policy).
- `cih-server/src/app/dispatch_tests.rs:37`: construct the test store via
  `connect_store` too (preserves the hermetic lazy-connect property the test
  relies on).
- Reword the doc comments that name `FalkorStore` generically
  (`app.rs:87,154,167`, `changes.rs:62`, plus the FalkorDB-specific wording in
  `cih-engine/src/db.rs` docs) so prose doesn't imply a hard-coded backend.

### Step 5 — Backend-neutral contract test suite
Add a `contract` test-support module to `cih-graph-store` (behind a
`test-support` feature), exposing
`run_contract_suite(mk: impl Fn(&str /*graph_key*/) -> anyhow::Result<Arc<dyn GraphStore>>)`.
The constructor is **key-parameterized** because the publish test builds a
store on `{key}-staging`, publishes, then constructs a second store for `key`
against the same backend instance (a Falkor closure captures the URL; a
path-based backend's closure captures a shared root dir). Coverage:
- `bulk_load` → `graph_summary`/`get_node`/`neighbors` round-trip
- `impact`, `call_chain`, `context`, `route_map`, `candidates_by_name`
- `upsert_incremental` (changed-file delete + reload semantics)
- `publish_to` + `drop_graph` (staging → live swap; and the Design-decision-3
  port guarantee: after `publish_to(dest)` returns, dropping the staging graph
  leaves the published graph fully queryable)
- `bulk_load_observed` fires `nodes_loaded`/`edges_loaded` in order (Falkor) or
  degrades to plain load (default impl)
Wire the existing Falkor integration tests to call the suite (runs when
FalkorDB on 6380 is up, as today — hermetic CI unaffected). M2's
`LocalGraphStore` runs the identical suite in-process.

### Step 6 — New-backend checklist (append to `docs/ARCHITECTURE.md`)
1. New crate implementing `GraphStore` (override `bulk_load_observed` only if
   the backend has phase events). 2. Feature + arm in `cih-store-factory`.
3. Contract suite green. 4. Nothing else — engine load, MCP tools, graph
browser, and background jobs all reach the store through the port.

## Files to modify
- `crates/cih-graph-store/src/lib.rs` — trait methods; contract module; delete
  `BulkLoader`.
- `crates/cih-falkor/src/{lib.rs,query.rs}` — inherents → trait impl; delete
  `FalkorBulkLoader`.
- `crates/cih-core/src/{lib.rs,artifacts.rs}` — reword `BulkLoader` doc
  comments.
- `crates/cih-store-factory/` — **new**.
- `Cargo.toml` — workspace member.
- `crates/cih-engine/src/db.rs` — generic load path.
- `crates/cih-engine/src/cmd/{args.rs,artifact.rs}`, plus backend threading in
  `analyze/mod.rs`, `discover.rs`, `cmd/taint.rs`.
- `crates/cih-server/src/{config.rs,app.rs,app/dispatch_tests.rs,changes.rs}` —
  factory-backed store creation; comment rewording.
- `docs/ARCHITECTURE.md` — checklist.

## Verification
1. Gates: `cargo build`, `cargo fmt --all --check`,
   `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo test --workspace` (hermetic suite green, no DB needed).
2. **Proof of decoupling:** `grep -rn "FalkorStore" crates --include='*.rs'`
   outside `crates/cih-falkor` matches only the single `cih-store-factory` arm
   — achievable because Step 4 also converts `dispatch_tests.rs` and rewords
   the doc comments that currently name `FalkorStore`.
3. **Behavior unchanged end-to-end** (FalkorDB on 6380, corpus copy in tmp):
   `cih-engine analyze <tmp> --all` shows the five load phases and loads the
   same node/edge counts; down-DB still fails fast (bounded connect timeout,
   exit 3); `--backend nosuch` errors listing compiled-in backends.
4. Contract suite green against Falkor locally.
5. MCP smoke: start `cih-server`, hit `list_repos` + `context` on an indexed
   multi-repo group — proves the factory-backed `store_for` path.

## Risks
- **Trait-object churn:** promoting `bulk_load_observed` moves it behind
  `async_trait` dynamic dispatch — negligible cost next to a bulk load, but the
  Falkor impl must keep its `&dyn LoadObserver` signature object-safe (it
  already is).
- **Hidden Falkor assumptions in the contract suite:** writing the suite may
  surface behaviors tools rely on that only Falkor guarantees (ordering,
  `LIMIT` semantics). Treat each as either a documented port guarantee (added to
  the suite) or a tool bug to fix — this discovery is a feature of the work.
- **Coordination with standalone M1:** land this first, then M1's `falkor`
  feature forwards to `cih-store-factory/falkor`; if M1 lands first, its cfg
  gates move from `cih-engine` internals into the factory crate during rebase.
