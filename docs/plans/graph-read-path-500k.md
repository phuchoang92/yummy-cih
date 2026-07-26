# Graph Read Path Performance and Semantic Hardening at 500k Scale

Status: revised against the current `dev` implementation on 2026-07-26.

## Summary

The OCB deployment has roughly 500,000 nodes and 1.5 million edges. BM25 search is already measured at that scale, but the FalkorDB read path has only been measured on the smaller Fineract graph. This plan establishes a representative, correctness-checked 500k graph workload before changing queries, then addresses the confirmed bottlenecks without duplicating the shared traversal logic.

The current code already has the backend-neutral bounded traversal in `cih-graph-store`: `flow_downstream` and `paths_between` use batched `execution_transitions`, retain equal minimum-depth predecessors, enforce node/edge budgets, preserve reverse route/listener evidence, and distinguish unreachable from budget-inconclusive results. This is the baseline architecture, not an in-flight precondition.

The implementation decisions are:

- Build a graph-specific 500k fixture with representative semantic islands; do not reuse the Method/CALLS-only search fixture as proof of route, database, or test behavior.
- Add real backend query timeouts and record correctness metadata with every latency sample.
- Keep `flow_downstream` and `paths_between` on the shared BFS. Treat FalkorDB `algo.SSpaths` as an impact-only experiment.
- Make the `Symbol(file)` index observable and prove that target queries use it.
- Scope semantic data by repository, graph artifact, and exact model identity before enabling hybrid search.
- Bound uncancellable embedding inference with a dedicated admission lane.
- Key graph-result caching by an authoritative generation published with the live graph, never by artifact-directory mtime.
- Run separate lexical, hybrid-semantic, and graph-read production gates.

No Symbol/edge data migration is required. Phase 4 adds backend lifecycle metadata for graph generations. Phase 1 introduces a versioned Postgres embedding schema and requires semantic re-embedding; that is not a graph re-index.

## Current confirmed hazards

| Hazard | Current implementation |
|---|---|
| `impact` enumerates variable-length CALLS paths before its result limit | `cih-falkor/src/query.rs::impact` |
| The 500k scale fixture contains only Method nodes and CALLS edges | `cih-server/src/scale_bench.rs` |
| `with_query_limit` bounds semaphore admission, not FalkorDB execution time | `cih-falkor/src/lib.rs` |
| Frontier lists and limits are inlined in current batched queries | `execution_transitions`, `interceptions_for_methods`, file/test/community reads |
| Required index DDL errors are discarded | Falkor `ensure_schema` and bulk-load index creation |
| The live graph has no authoritative generation readable by the server | graph publish lifecycle |
| Semantic rows are not repository-, artifact-, or model-scoped | `cih-embed/src/store.rs` |
| Server inference is hard-coded to MiniLM while the CLI also permits BGE | `cih-server/src/bootstrap.rs`, `cih-engine` embed command |
| Embedding inference runs inline on the async executor: `EmbedModel::embed` is a synchronous fn holding a `std::sync::Mutex`, awaited with no `spawn_blocking` and no lane, so it stalls the worker thread, serializes all callers, and a caller timeout abandons but cannot cancel it | `EmbedModel::embed`, `SearchState::query_hits` |
| Existing production soak is intentionally lexical-only and does not call graph traversal tools | retrieval acceptance scripts |

## Global measurement rules and SLOs

Every benchmark row must report both performance and correctness. A faster empty, stale, wrong, or budget-truncated result is a failure, not an optimization.

Each row records:

- 50 warm samples per run and three independent release-mode runs;
- p50, p95, p99, minimum, maximum, timeout count, and error count;
- result count, stable result digest, and the exact seed IDs;
- traversal status, `visited_nodes`, `expanded_edges`, `truncated`, and `has_more` where applicable;
- commit, fixture schema/digest, graph counts, degree summary, FalkorDB version/image digest, `THREAD_COUNT`, `CACHE_SIZE`, query timeout, host profile, and warmup policy.

Initial product SLOs are absolute gates in addition to phase-specific relative improvements:

| Operation class | Warm p95 gate |
|---|---:|
| Indexed name/file lookup | <= 250 ms |
| `graph_summary` | <= 1 s |
| Default `graph_overview` | <= 2 s |
| Representative complete `impact`, `trace_flow`, and `reaches` | <= 2 s |
| Complete hub/unreachable bounded cases | <= 5 s |
| Mixed MCP graph workload at selected production concurrency | <= 5 s |
| Hybrid search with a healthy semantic backend | <= 2 s |

No individual benchmark query may run beyond a 10-second backend-enforced timeout. Phase 0 may tighten these thresholds. Relaxing one requires committed host evidence and an explicit plan revision; a relative speedup alone cannot make an unusable latency acceptable.

## Phase 0 - Trustworthy 500k graph baseline

### 0a. Dedicated graph workload fixture

Add `ensure_graph_fixture` beside the existing search fixture rather than changing the byte-stable search fixture. Generate approximately 500,000 nodes and 1.5 million edges plus deterministic workload islands containing:

- Route -> handler -> service CALLS chains;
- publisher -> Kafka topic and topic -> listener logical execution flow;
- equal shortest read and write paths to the same `DbTable`;
- `TESTS` edges, changed-file seeds, and covered/uncovered production symbols;
- promoted nonzero complexity properties;
- cycles, equal-shortest diamonds, a disconnected target, a broad frontier, and high fan-in hubs;
- structural nodes needed by the default graph overview.

Write a seed manifest naming every benchmark endpoint and its expected status, cardinality, depth, final edge, and completeness. Include the workload profile, overlay settings, and seed-manifest digest in `FixtureMetadata`; fixture reuse must fail when any of them differs.

### 0b. Safe fixture loader

Add `crates/cih-server/examples/graph_scale_load.rs` and connect through `cih-store-factory`.

- Use a unique graph key derived from the fixture digest, or require an explicitly verified empty benchmark key.
- Report whether the adapter used native `GRAPH.BULK` or the Cypher upsert path.
- Never silently reuse a populated `scale500k` key, because `bulk_load_observed` then selects the Cypher `MERGE` upsert fallback (`load_nodes_edges`) instead of native `GRAPH.BULK`.
- Create and verify required indexes after the load.
- Cleanup may target only the exact generated benchmark key.

### 0c. Adapter and MCP benchmarks

Keep `cih-falkor/examples/query_bench.rs` as the low-level adapter microbenchmark and add a `cih-server` benchmark/runner for application and MCP behavior. Benchmark current API names and semantics:

- `impact`: upstream depths 2/4/6/8, both-direction hub case, cycle case;
- `execution_transitions`: route, method, topic, and database frontiers;
- `flow_downstream`: route and method roots, filters, pagination, budget case;
- `paths_between`: reachable read, reachable write, equal shortest paths, complete unreachable, and budget-inconclusive;
- `nodes_in_files`, `tests_for_files`, `untested_symbols`;
- `graph_summary`, `graph_overview`, `complexity_hotspots`, `subgraph`;
- name lookup, context, and `detect_changes` fan-out.

Add a Falkor read-query execution timeout that is passed as the `GRAPH.QUERY TIMEOUT` argument. Keep semaphore admission timeout separate and report them separately. The benchmark must collect every `JoinSet` result and fail on task or query errors; expected execution timeouts are censored samples, not discarded failures.

The MCP runner must include symbol resolution, database-effect enrichment, serialization, and the query semaphore so direct adapter timing is not mistaken for end-to-end tool latency.

### 0d. Baseline records

Commit:

- `docs/perf/graph-read-path-500k.json` for the synthetic fixture;
- a sanitized before-change OCB report using explicit known seed IDs;
- a result/provenance schema shared by all later runs.

Warm each BM25 index once before reading `/operations/metrics`. Size retained search cache from the sum of hot `index_bytes` plus 10% headroom. Size the cold lane from its actual reservation formula, not retained size alone: at least `max(2 * sidecar_payload_bytes, 1.25 * retained_index_bytes)` plus headroom.

### Phase 0 exit criteria

- Every named row runs against representative nonempty data.
- Reachable, complete-unreachable, and expected budget-inconclusive cases match the seed manifest.
- There are no discarded task/query errors.
- Backend timeouts terminate server work and are recorded per row.
- Synthetic and pre-change OCB reports are committed before optimization begins.
- Phase 2 index memory/build caps and per-row improvement thresholds are declared numerically in the baseline report.

## Phase 1 - Offline, scoped, bounded semantic search

Phase 1 is independent of graph query optimization, but hybrid search must not be enabled until its identity and isolation rules are implemented.

### 1a. Canonical offline model cache

Make `CIH_EMBED_CACHE_DIR` the CIH-facing setting and migrate the Dockerfile, Compose, environment documentation, and runbooks away from setting `HF_HOME` directly.

Compatibility rules:

- only `CIH_EMBED_CACHE_DIR`: pass it to fastembed;
- only `HF_HOME`: use it with a deprecation warning;
- both set to the same canonical path: accept;
- both set differently: fail with an actionable configuration error.

Add `cih-engine model prefetch --model <name> --cache-dir <path> --json`. It downloads the exact fastembed model repository into the resolved cache and reports model repository, revision/fingerprint, and files. It does not require a repository or Postgres connection. The air-gapped runbook copies that complete cache to a read-only volume and sets offline mode.

Tests run in a network-disabled container:

- a complete read-only preseeded cache loads successfully;
- an absent/incomplete cache fails without a network attempt and names the model, cache setting, and prefetch command;
- conflicting cache settings fail before model initialization.

### 1b. Repository, artifact, and model identity

Introduce versioned Postgres semantic tables rather than reading legacy unscoped rows. Every row/generation is keyed by:

- stable `repo_id` (the registered graph key);
- graph artifact version;
- exact model repository and revision/fingerprint;
- embedding schema version;
- node ID and chunk index.

Add `CIH_EMBED_MODEL`, defaulting to `all-minilm-l6-v2`, and make CLI/server model selection use the same parser. Persist the selected identity and refuse dimension-only compatibility. Include model/schema identity in the chunk hash so a model change cannot skip unchanged text.

`cih embed` must receive or unambiguously resolve the repository graph key. Build a new semantic generation, mark it complete only after all chunks and node vectors succeed, then atomically make it current and prune older generations. Failed runs leave the previous complete generation intact. Scope exact/ANN counts, reads, writes, and pruning by repository and generation; do not pass all 500k IDs in one SQL array.

The server queries semantic data only when a complete generation matches the resolved repository, current artifact version, and configured model fingerprint. Existing unscoped rows are treated as legacy/unavailable. Enabling this phase therefore requires a semantic rebuild, not a graph re-index.

### 1c. Bounded hybrid execution

Introduce a mockable `SemanticSearchProvider` port instead of storing a concrete `EmbedStore` in `SearchState`.

- Use a dedicated semantic inference lane, default concurrency 1, with bounded admission.
- Acquire the permit before `spawn_blocking` and move it into the blocking closure so a caller timeout does not release capacity while uncancellable inference is still running.
- Add separate queue, inference, Postgres connect, and Postgres statement deadlines. Default total semantic deadline: 1500 ms.
- Cache exact-versus-ANN routing per repository/generation with a TTL because another process may publish embeddings.
- Cache only successful routing metadata.

Failure policy:

- if lexical search is configured and succeeds, semantic error/timeout returns lexical hits and increments degradation metrics;
- semantic-only mode returns a retryable unavailable error, never an empty successful result;
- with lexical search configured, semantic initialization failure starts the server in an explicit degraded state and retries on a bounded cooldown;
- with semantic-only configuration, failed initialization prevents readiness.

Metrics include attempted, succeeded, degraded-error, degraded-timeout, active, queued, rejected, elapsed time, readiness state, repository scope, and model fingerprint.

### Phase 1 tests and exit criteria

- Model mismatch forces re-embedding and cannot mix vector spaces.
- Two repositories with colliding node IDs remain isolated.
- Stale chunks and node vectors are pruned only after successful publication.
- Semantic-only failure is an error; hybrid failure preserves lexical results.
- A timeout burst never exceeds the configured running inference concurrency.
- Offline startup succeeds from the exact read-only cache used by the deployment image.

## Phase 2 - Observable indexes and stable query plans

### 2a. Required file index

Centralize Falkor required-index creation so `ensure_schema`, Cypher bulk/upsert, and native bulk completion use the same definition. Add `CREATE INDEX FOR (n:Symbol) ON (n.file)`.

Do not discard arbitrary DDL errors. Handle an already-existing index idempotently, then verify required indexes with `CALL db.indexes()` and require operational status. A legacy graph remains queryable if index creation fails, but startup/benchmark reports the graph as degraded and Phase 2 acceptance fails.

Use `GRAPH.EXPLAIN`/`GRAPH.PROFILE` to prove an index scan for:

- `nodes_in_files` and incremental changed-file deletion;
- both `tests_for_files` query shapes;
- `untested_symbols` prefix lookup.

If relationship-first test queries prevent index anchoring, rewrite them to start from indexed production symbols while preserving result parity.

### 2b. Parameterization

Extend the existing `CYPHER k=<literal> ... $k` preamble idiom — already used by `impact`, `subgraph`, `call_chain`, `get_node`, `neighbors`, `route_map`, `candidates_by_name`, and others — to the hot queries that still inline whole ID lists and limits via `format!`. Note the existing idiom formats the `cstr()`-escaped literal into the preamble string rather than sending out-of-band parameters, so `cstr()` escaping remains load-bearing for injection safety; the goal is a stable query body behind the preamble, not binary parameter transport. Targets:

- `execution_transitions` forward and reverse batches;
- `interceptions_for_methods`;
- incremental file deletion and `nodes_in_files`;
- `processes_for_symbols`, `db_effects_for_methods`, `symbol_communities`;
- both `tests_for_files` queries and `untested_symbols` prefix/limit;
- graph-overview selected kinds/IDs and other measured varying lists/limits.

Relationship labels and variable-length depth bounds remain validated literals where FalkorDB cannot parameterize grammar.

The live Falkor precheck runs two distinct parameter sets and proves:

- identical result checksums/cardinalities versus the prior query;
- a stable query body;
- the second execution reports a cached plan.

### Phase 2 acceptance

- Required indexes exist and are operational.
- Targeted plans use the file index where supported.
- Result digests and cardinalities are unchanged.
- Index build time and graph-memory delta stay below the numeric caps committed in Phase 0.
- No targeted row regresses by more than 5% p95.
- At least one targeted row meets its predeclared Phase 0 improvement threshold; otherwise keep only the correctness/plan-stability changes that have negligible cost.

## Phase 3 - Bounded impact traversal; preserve shared flow/path traversal

`flow_downstream` and `paths_between` remain the backend-neutral defaults. Do not reintroduce Falkor-only filtering, pagination, path reconstruction, AOP enrichment, or traversal-budget logic.

### 3a. Shared bounded impact fallback

Add a deterministic batched CALLS-neighbor primitive to `GraphStore`. A batched one-hop primitive already exists — `execution_transitions` — but it cannot serve impact as-is: it has no upstream (reverse-CALLS) direction and matches `CALLS|EXTERNAL_CALL|PUBLISHES_EVENT` plus reverse route/listener edges rather than CALLS only. Either generalize `execution_transitions` with direction and edge-kind selection or add a sibling primitive that shares its batching and query shape; do not let the backends grow two near-duplicate batched neighbor queries. Implement one-hop indexed reads in Falkor and Ladybug and put impact BFS, cycle handling, stable ordering, depth semantics, and budgets in `cih-graph-store`.

Extend `Impact` compatibly with traversal statistics and an explicit completeness/status marker so a capped or budget-truncated result is not presented as a complete blast radius. Preserve the existing affected-node fields and risk label.

Contract cases cover:

- upstream, downstream, and both directions;
- cycles and minimum depth;
- deterministic parent selection on a diamond;
- result cap, node budget, and edge budget;
- complete empty versus budget-inconclusive;
- parity of IDs, depth, parent, and risk for small complete fixtures.

### 3b. Falkor `algo.SSpaths` impact-only spike

Time-box the spike to one day. Do not assume `pathCount: 1` means one path per destination; it limits the total paths returned.

The procedure is eligible only for CALLS-only `impact`, and only if it proves:

- unweighted minimum-hop semantics;
- one deterministic parent per affected node;
- unique destination handling and the 200-result compatibility cap;
- all three directions;
- traversal/resource bounds and honest incompleteness;
- exact contract parity.

Kill the procedure path if it is absent on the deployed Falkor version, cannot expose required parent/depth/budget information, violates parity, or provides less than 1.5x p95 improvement at depth 6/8. The shared batched BFS is the required fallback.

Do not replace `paths_between` with `algo.SPpaths`: one global relationship direction cannot directly express forward calls/database edges plus reverse route/listener transitions, and the shared implementation already owns equal-shortest-path and final-edge access semantics.

### Phase 3 acceptance

- Exact impact result parity for complete fixtures.
- Complete-unreachable and budget-inconclusive remain distinct across graph tools.
- Route handler remains depth 1; topic -> listener retains reverse evidence.
- Equal shortest paths, read/write final-edge filtering, hidden-node traversal, and pagination remain unchanged.
- Impact depth 6/8 improves by at least 1.5x p95 on identical verified output.
- Depth 2 and existing shared flow/path rows regress by no more than 5% p95.

## Phase 4 - Live-generation-keyed graph result caching

Artifact directories are not graph generations. Artifacts may appear before a graph publish, remain after `--no-load` or load failure, and base artifacts do not change when discover republishes base plus community data. Do not use `GraphArtifacts::latest_in_dir` in graph cache keys.

### 4a. Authoritative graph generation

Add opaque `GraphGeneration` lifecycle support:

- each successful staging publication receives a fresh unique generation, including repeated publication of identical content, and uses a staging key derived from that generation rather than the shared `<graph>-staging` key;
- `GraphStore::graph_generation()` reads the live generation;
- `publish_to` accepts the generation and publishes graph data plus generation atomically;
- Falkor performs graph `RENAME` and companion generation-key update in one Redis transaction;
- Ladybug stores version plus generation in one backward-compatible `CURRENT` pointer and publishes both with its existing atomic pointer flip;
- metadata is excluded from Symbol queries and graph counts;
- legacy graphs without a generation remain fully queryable but bypass result caching until the next successful load.

Update every production publisher (`load_many`, `load_with_progress`, discover/community publication, artifact bootstrap, and fixture loading) to use the generation-aware staging API. `ArtifactCommand::Bootstrap` must delegate to the shared staging loader instead of bulk-loading the live key. Production code must not mutate a live cache-eligible graph in place; a maintenance/test-only direct write first removes its generation and leaves the graph uncached until a successful staging publication.

The backend contract proves that a successful publish changes generation and that a failed load/publish leaves both the prior graph and generation live. The live Falkor integration must also prove that the deployed Redis/Falkor combination permits graph `RENAME` and companion generation update in one `MULTI`/`EXEC`. If it does not, graph-result caching stays disabled for Falkor until an atomic mechanism is implemented; do not ship a non-atomic fallback.

### 4b. Ownership and behavior

Create one process-wide `GraphResultCache` from validated configuration. Keep raw graph connections cached by graph key. On each repository resolution, read the live generation once and return a lightweight `CachedGraphStore` containing the raw store, graph key, generation, and shared cache.

- Missing/failed generation lookup is fail-open: count/warn and execute uncached.
- Cache successful results only.
- Coalesce concurrent misses with a new non-retaining flight primitive; do not reuse the persistent connection `SingleFlight`.
- Before inserting a miss result, re-read generation and cache only when it is unchanged.
- Forward all uncached methods exactly, including `bulk_load_observed`.
- Writes invalidate the affected graph; `publish_to` also invalidates the destination.
- Purge old generations when a new generation is observed.
- `/ready` always uses the raw store so cached data cannot hide a backend outage.

Initial scope is repository/MCP services. The primary HTTP graph-browser service remains raw unless separately refactored to resolve a fresh repository context per request.

Cache key: `(graph_key, generation, typed_method, owned_canonical_arguments)`.

- Tier A: `candidates_by_name`.
- Tier B: `graph_summary`, `graph_overview`, `communities`, `route_map`.
- Tier C remains deferred until repeat-call metrics prove value for impact/trace results.

Use exact defaults:

- `CIH_GRAPH_RESULT_CACHE_MAX_BYTES=33554432` (32 MiB);
- `CIH_GRAPH_RESULT_CACHE_MAX_ENTRIES=4096`;
- increase the default total cache ceiling from 1040 MiB to 1072 MiB.

Weight keys, values, vector/string capacities, nodes, and slot overhead. Oversized values are served but not retained. Metrics are bounded and per method: requests, hits, misses, flight joins, invalidations, generation bypasses, retained entries/bytes, evictions, and oversize results.

### Phase 4 tests and acceptance

Unit tests cover hit/miss, non-retaining single flight, errors not cached, canonical arguments, graph isolation, generation bump, generation race, legacy bypass, purge, eviction, oversize, mutation/destination invalidation, and observer forwarding.

Add cacheable reads before and after incremental mutation and publication to the backend-neutral contract, then run the decorated contract hermetically on Ladybug and in the mandatory live Falkor suite.

Put cache benchmarks under `cih-server` so they can wrap stores from `cih-store-factory` without a reverse dependency. Compare raw, first miss, and warm hit for Tier A/B methods, including clone cost and an alternating multi-repository workload. Ship a tier only when it has repeatable warm p95 benefit and no measurable uncached regression.

## Phase 5 - Separate production gates and closeout

### 5a. Lexical retrieval gate

Keep `validate-retrieval-production.py` and the existing eight-repository 30-minute soak explicitly lexical-only. Record cold/warm search latency, cache sizing, RSS, reloads, evictions, and event-loop health. Run the eight-repository gate only with eight distinct production repositories.

### 5b. Hybrid semantic gate

Add a semantic-enabled acceptance/soak runner that verifies:

- network-disabled startup from the read-only preseeded cache;
- expected model fingerprint and matching semantic generation;
- repository isolation, including colliding node IDs;
- nonempty semantic participation and stable hybrid result digests;
- zero unexpected degradation under healthy conditions;
- injected Postgres timeout/outage preserves lexical results and increments degradation counters;
- semantic-only failure remains unavailable.

Gate on measured hybrid p95 and semantic success/degradation rates; do not reuse lexical-only thresholds blindly.

### 5c. Graph MCP gate

Add a separate MCP-level graph runner for `impact`, `trace_flow`, and `reaches`, using known route, method, table, unreachable, broad-frontier, and hub seeds.

- Validate result digests/cardinalities, traversal status, budgets, pagination, timeout counts, and completeness.
- Run concurrency levels 1, 2, 4, 8, 16, 32, and around the deployed Falkor `THREAD_COUNT` when different.
- Select `CIH_MAX_CONCURRENT_QUERIES` from the measured throughput/latency knee; equality with `THREAD_COUNT` is only a starting hypothesis.
- Require zero unexpected errors and event-loop health p99 below 50 ms.

Use Fineract as the local correctness smoke, then run the same before/after graph benchmark and soak against real OCB. Pin the FalkorDB image or record and reuse the exact image digest and configuration for every comparison. Live Falkor validation is mandatory and must not be silently skipped.

### 5d. Documentation and rollback

Update architecture, environment, security, and multi-repository runbooks with:

- shared traversal and impact completeness semantics;
- backend query timeout and concurrency settings;
- required graph indexes and verification commands;
- live graph generation/cache behavior;
- semantic repository/model/artifact isolation and offline prefetch procedure;
- separate lexical, hybrid, and graph acceptance commands;
- independent rollback switches for semantic search, graph result caching, and graph query concurrency.

Commit separate lexical, hybrid, and graph production reports containing commit, release profile, artifact/generation identity, graph counts, Falkor version/configuration, host profile, exact limits, sample counts, timeouts, and result cardinalities. Reports must not contain source text, raw production selectors, or credentials.

## Sequencing

1. Phase 0 fixture, safe loader, adapter/MCP harness, synthetic baseline, and pre-change OCB baseline.
2. Phase 2 indexes/parameterization and Phase 1 semantic correctness may proceed independently after their Phase 0 measurements exist.
3. Phase 3 impact traversal starts after Phase 2 so it measures stable batched queries.
4. Phase 4 graph generation lands before result caching; cache tiers remain independent changes.
5. Phase 5 runs after the corresponding phase passes local and live-backend gates.

Every phase is independently revertible and must include its own before/after evidence.

## Interface and migration changes

| Surface | Change |
|---|---|
| `GraphStore` | Add batched CALLS/general neighbor primitive and live generation read; publish accepts `GraphGeneration` |
| `Impact` | Add backward-compatible traversal/completeness metadata |
| Falkor/Ladybug lifecycle | Atomically publish live graph generation; legacy missing generation bypasses caching |
| Falkor queries | Required `Symbol(file)` index, stable parameters, backend read timeout |
| Server cache config | Add exact graph result byte/entry budgets; total default becomes 1072 MiB |
| Semantic Postgres | New repository/artifact/model-scoped generation schema; legacy unscoped data ignored |
| Semantic server config | Add `CIH_EMBED_MODEL`, canonical cache resolution, bounded inference/deadline settings |
| CLI | Add offline `cih-engine model prefetch` with machine-readable model identity and cache manifest |
| Production validation | Separate lexical, hybrid, and graph runners/reports |

## Explicit deferrals

| Deferred | Reactivation trigger |
|---|---|
| `subgraph` untyped/undirected traversal rewrite | Verified `subgraph_r4` p95 exceeds 500 ms and browser usage is material |
| `call_chain` rewrite | Phase 0 shows it violates an absolute SLO on a real workflow |
| mmap/segmented BM25 index | Real retained index exceeds cold/retained capacity after correct sizing |
| Graph artifact snapshot compaction | Host RSS evidence shows artifact snapshots are the limiting resident set |
| Tier C impact/trace result caching | Per-method repeat-call metrics show material identical-call traffic |
| PostgreSQL connection pool | Statement/connect failures or concurrency measurements show the single client is limiting |
| Ladybug production-specific tuning | Ladybug becomes a supported production backend |

## Verification commands

For every implementation phase:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

For graph-store, query, generation, or cache changes, also run the hermetic Ladybug contract and the ignored live Falkor integration/contract suite. For semantic changes, run Postgres integration tests plus the network-disabled model-cache test. Production gates are required only on the target host and must be recorded as external evidence rather than claimed from local tests.

## Critical files

- `crates/cih-server/src/scale_bench.rs` and new graph benchmark/loader runners
- `crates/cih-falkor/src/{lib,query}.rs` and `examples/query_bench.rs`
- `crates/cih-graph-store/src/{lib,contract,traversal}.rs`
- `crates/cih-ladybug/src/{query,schema}.rs`
- `crates/cih-server/src/infrastructure/{repo_context_provider,search_provider}.rs`
- `crates/cih-server/src/infrastructure/cache/`
- `crates/cih-server/src/config.rs` and retrieval metrics
- `crates/cih-embed/src/{model,store,text}.rs`
- `crates/cih-engine/src/analyze/`, `crates/cih-engine/src/{discover,db,embed}.rs`, and `crates/cih-engine/src/cmd/{args,artifact,model}.rs`
- production acceptance/soak scripts and `docs/perf/`
