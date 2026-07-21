# CIH Server Gap Closure Plan

> **Status:** IMPLEMENTED - 2026-07-21; compact/memory-mapped storage remains a
> measured follow-up because persisted indexes improve restart latency but do
> not reduce the 500k snapshot's resident size
> **Parent plan:** `docs/plans/cih-server-clean-architecture-and-scalability.md`
> **Scope:** Close the remaining Milestone 5 findings and Definition of Done
> gaps for `cih-server`.

## 1. Objective

Complete the remaining scalability and operational work without weakening the
clean architecture already established in `cih-server`.

The closure effort must make the server predictable for repositories with
500k+ graph nodes and multiple services while preserving the existing MCP and
HTTP contracts. The work must improve memory behavior, tail latency,
observability, and correctness metadata rather than only adding benchmark
fixtures.

The target outcome is:

```text
large repository
  -> bounded indexed storage and search
  -> predictable cache/admission behavior
  -> typed completeness and overload metadata
  -> measurable request, queue, and cache lifecycle
  -> scheduled multi-repository soak validation
```

## 2. Current Gaps

The audit identified these remaining gaps:

1. BM25 p95 at 500k documents is approximately 489-511 ms and can exceed the
   500 ms acceptance target.
2. A 500k-node artifact snapshot and BM25 index are individually larger than
   the default artifact and search cache budgets, so they are served through
   oversize bypass instead of being retained.
3. Artifact indexes are built in memory after parsing JSONL. Persisted or
   memory-mapped indexes do not exist yet.
4. Cross-repository tracing still uses artifact snapshots for the repository
   graph walk. More reads have not been evaluated or moved to `GraphStore`.
5. Wiki search has incremental generation support, but page-level materialized
   serving and invalidation are not complete as a server-side read strategy.
6. There is no scheduled multi-service soak test covering memory stability,
   cache churn, queueing, and concurrent cold requests.
7. There is no structured per-request completion event with request ID,
   capability, duration, queue wait, result count, response size, completeness,
   and error classification.
8. Cache metrics exist, but blocking-lane and index-queue active/queued/
   rejected counters are not exposed as operational metrics.
9. `Completeness` is implemented for `detect_changes`, but graph operations
   that truncate by a result limit do not consistently tell clients that the
   result is bounded rather than exhaustive.
10. Process execution behavior is implemented inside the local scheduler, but
    there is no separate `EngineProcessRunner` port and reusable contract suite.
11. `architecture_overview` and repository `status` still perform a few small
    synchronous sidecar/stat reads from async paths. These are currently
    accepted as low-cost, but need an explicit policy and regression guard.

## 3. Non-Goals

- Do not replace MCP or Axum.
- Do not split `cih-server` into deployable microservices.
- Do not change graph-analysis semantics or parser output as part of this work.
- Do not raise cache budgets blindly to make the benchmark pass.
- Do not expose raw internal metrics with unbounded repository-name labels.
- Do not make every graph response claim completeness when the backend itself
  provides a bounded result.

## 4. Design Principles

### 4.1 Measure before changing representation

Every optimization must have a before/after measurement for:

- p50, p95, p99 latency;
- peak RSS and retained cache weight;
- event-loop delay;
- loader/build counts under concurrency;
- queue wait and overload rejection;
- result count, bytes, and completeness state.

### 4.2 Keep application contracts independent of metrics and storage

Application services should return typed domain/application results. Transport
adapters map those results to MCP/HTTP. Infrastructure owns implementation
metrics, but metrics must be emitted through a small server-owned observability
port rather than imported directly into use cases.

### 4.3 Separate active memory from retained memory

A request may temporarily borrow a large snapshot even when the cache cannot
retain it. Reports must distinguish:

- retained cache bytes;
- active borrowed bytes;
- peak process RSS;
- oversize bypass count.

This prevents a false conclusion that a bounded cache makes arbitrary active
work safe without admission control.

### 4.4 Completeness must describe the answer, not the algorithm only

Every bounded result must identify whether it is:

- complete for the requested scope;
- truncated by an explicit result limit;
- partial because work failed or was omitted;
- unavailable because an upstream dependency failed.

## 5. Target Architecture Additions

Add the following boundaries without changing the existing transport shape:

```text
transport
  -> application services
       -> domain + ports
            -> infrastructure adapters

application -> ObservabilityPort
infrastructure -> ObservabilityPort
infrastructure -> EngineProcessRunner
infrastructure -> ArtifactIndexStore
```

### 5.1 New ports

#### `ObservabilityPort`

Responsibilities:

- record request completion events;
- record blocking-lane state transitions;
- record index queue state transitions;
- record cache and oversize events where an existing cache metric is not
  sufficient.

The port must accept bounded, typed fields. Repository identity should be a
controlled identifier or omitted from metric labels.

#### `EngineProcessRunner`

Responsibilities:

- start an indexing command;
- stream bounded stdout/stderr;
- report exit, timeout, cancellation, and truncation;
- terminate the child on deadline or cancellation;
- carry an allowlisted environment and explicit working directory.

The current local scheduler behavior becomes the first adapter. The scheduler
should depend on the port, not implement process I/O itself.

#### `ArtifactIndexStore`

Responsibilities:

- load or build node-ID and adjacency indexes for one artifact version;
- validate index schema and source-file identities;
- persist indexes atomically;
- memory-map or stream immutable index sections when possible;
- invalidate indexes when nodes, edges, or schema versions change.

The port must support an in-memory fallback for compatibility and tests.

#### `WikiMaterializationStore`

Responsibilities:

- serve page metadata and page bodies by stable page ID;
- publish a new version atomically;
- invalidate or replace only changed pages;
- provide a version token for resource/search cursors.

The current filesystem wiki bundle is the first adapter.

## 6. Workstream A: Observability and Completion Events

### A1. Define typed events

Add a server-owned completion event with:

```text
request_id
transport
capability
repository_id
duration_ms
queue_wait_ms
result_count
response_bytes
completeness
error_kind
```

Fields must be optional where a transport cannot provide them, but every
application request must emit one terminal event: success, partial result,
validation failure, overload, timeout, or dependency failure.

### A2. Add request context at transport boundaries

MCP and HTTP adapters create a request context once and pass it through the
application call. The context must not leak protocol types into application or
domain modules.

For MCP, use the request/task correlation available at the transport boundary.
For HTTP, use an incoming request ID when present or generate one. Never log
authentication tokens or raw request bodies.

### A3. Instrument queue and cache lifecycle

Expose bounded counters/gauges for:

- blocking lane active permits;
- blocking lane queued waiters;
- blocking lane rejections;
- blocking timeouts and panics;
- index queue depth;
- index jobs running;
- index queue rejections;
- index job duration by terminal reason;
- oversize artifact/search/wiki bypasses;
- retained cache bytes by cache family.

Use controlled labels such as capability, terminal reason, and cache family.
Do not use arbitrary repository names as metric labels.

### A4. Acceptance criteria

- Every MCP and HTTP application request emits exactly one terminal event.
- An overload response increments the relevant rejection counter.
- Queue wait is measurable separately from execution time.
- Metrics remain bounded under arbitrary repository names and request IDs.
- Existing cache metrics remain compatible or are mapped into the new event
  vocabulary.

### A5. Tests

- success, validation failure, partial result, timeout, overload, and backend
  failure each emit one terminal event;
- concurrent blocking calls produce correct active/queued/rejected counts;
- concurrent index submissions produce correct queue depth and running count;
- a high-cardinality repository set does not create unbounded metric labels;
- response byte counting matches the actual MCP/HTTP serialized payload.

## 7. Workstream B: Uniform Completeness Metadata

### B1. Define reusable result metadata

Extend the existing `Completeness` type with a clearly defined bounded-result
variant, or add a sibling `ResultBounds` type, containing:

```text
complete
total_known
returned
omitted
failed
limit
reason
```

Do not overload `failed` for intentional result truncation. Use explicit reasons
such as `result_limit`, `byte_budget`, `symbol_budget`, `dependency_failure`,
and `timeout`.

### B2. Apply it to graph capabilities

Review and update:

- `impact`;
- `communities`;
- `complexity_hotspots`;
- `find_duplicates`;
- `route_map`;
- `trace_flow`;
- cross-repository trace outputs;
- taint and testing outputs where an analysis cap exists.

Each result must preserve current fields and add metadata in a compatibility-
safe way. Existing clients must continue to parse the response.

### B3. Define backend semantics

Document whether `total_known` means:

- an exact backend count;
- a count observed before truncation;
- an unknown total.

Never infer completeness merely because the returned vector length is below the
requested limit unless the backend proves there are no more results.

### B4. Acceptance criteria

- A result capped by an explicit limit cannot serialize as complete.
- A backend failure is distinguishable from intentional truncation.
- Deterministic ordering remains unchanged.
- Golden JSON tests cover complete, limit-truncated, byte-truncated, timeout,
  and dependency-failure variants.

## 8. Workstream C: Persisted and Memory-Mapped Artifact Indexes

### C1. Define an index format

Create a versioned immutable artifact-index format containing:

- artifact schema version;
- source nodes/edges file identities;
- node ID to ordinal mapping;
- outgoing and incoming adjacency ranges;
- optional compact node/edge offset tables;
- checksum and format version.

Use fixed-width offsets and lengths where practical. Keep the format portable
across 64-bit platforms and reject incompatible versions safely.

### C2. Build indexes during analysis or first server load

Preferred order:

1. `cih-engine analyze` writes the index beside the JSONL artifacts when it has
   enough information to do so cheaply;
2. `cih-server` lazily builds the index when absent;
3. server-built indexes are written to a temporary path and atomically renamed;
4. a lock or single-flight gate prevents duplicate builders;
5. a failed build leaves no partial index that can be mistaken for valid data.

### C3. Add an in-memory fallback

The fallback is required for:

- old artifact directories;
- read-only artifact mounts;
- incompatible index versions;
- tests and development fixtures.

The fallback must continue to enforce the blocking lane and cache budgets.

### C4. Reduce memory duplication

Measure three modes:

- current JSONL parse plus in-memory indexes;
- persisted indexes plus parsed nodes/edges;
- persisted indexes with memory-mapped immutable sections.

Do not memory-map blindly. Keep frequently accessed node metadata resident only
if measurements show it improves p95 without exceeding active-memory limits.

### C5. Acceptance criteria

- A warm server restart can reuse a valid persisted index without rebuilding
  adjacency maps.
- Changing either `nodes.jsonl` or `edges.jsonl` invalidates the index.
- A partial or corrupt index is ignored and rebuilt safely.
- Peak RSS for the 500k-node fixture is reduced or the result is documented as
  an explicit accepted limit with a justified budget.
- Same-key concurrent index loads still produce one builder.

### C6. Tests and benchmarks

- format round-trip and schema-version rejection;
- checksum and source-identity invalidation;
- atomic-write failure recovery;
- concurrent builder coalescing;
- read-only directory fallback;
- cold/warm load latency and peak RSS in `scale_bench`.

## 9. Workstream D: BM25 and Search Scalability

### D1. Profile the query path

Measure separately:

- query tokenization;
- term lookup;
- document scoring;
- result heap/sorting;
- response construction;
- cache hit and cache miss behavior.

Use the 500k fixture and at least three query shapes: common terms, rare terms,
and multi-term queries.

### D2. Optimize based on the profile

Evaluate, in order:

1. avoid rebuilding per-document term-frequency structures;
2. use posting lists and candidate intersection to score only matching docs;
3. use a bounded top-k heap instead of sorting all candidates;
4. reuse tokenized query terms and normalized document metadata;
5. reduce allocations in `SearchHit` construction;
6. consider a persisted search index for warm restarts.

Do not change ranking semantics without golden ranking tests.

### D3. Acceptance criteria

- 500k-document search p95 is below 500 ms on the documented reference
  machine, or the target is explicitly revised with evidence;
- ranking remains identical for existing deterministic fixtures;
- cold-build and warm-query memory are separately reported;
- oversize cache behavior remains bounded.

## 10. Workstream E: Cross-Repository Graph Strategy

### E1. Classify cross-repo operations

For each operation, record whether it needs:

- raw nodes/edges and local adjacency;
- contract metadata only;
- graph-store traversal;
- a hybrid of artifact and graph-store data.

Initial candidates:

- contract matching and shape checks: retain artifact-backed path where it is
  deterministic;
- cross-repo trace first leg: evaluate `GraphStore` traversal for high-degree
  nodes;
- route and caller lookups: evaluate store-side bounded traversals;
- artifact-only fallback: retain for offline and read-only operation.

### E2. Add a strategy port

Introduce a port that can choose artifact, GraphStore, or hybrid traversal based
on repository capabilities and query budgets. The application service must not
know which strategy was selected.

### E3. Acceptance criteria

- output identity and ordering remain compatible across strategies;
- each strategy has bounded depth, node, edge, and response budgets;
- unavailable GraphStore data falls back explicitly rather than silently
  returning an incomplete answer;
- benchmark compares memory, latency, and correctness on multi-repository
  fixtures.

## 11. Workstream F: Page-Level Wiki Materialization

### F1. Define page identity and versioning

A page identity consists of repository, page kind, stable page ID, and source
artifact version. A page version is immutable after publication.

### F2. Incremental generation

On a new artifact/wiki version:

1. calculate changed communities/modules/pages;
2. regenerate only affected pages;
3. write changed pages to a staging directory or store;
4. atomically publish the new manifest and page index;
5. retain old versions only according to an explicit TTL/cap.

### F3. Server read path

Wiki search should query page metadata/indexes first. Page reads should seek to
one page by stable ID without scanning the entire bundle. MCP resources and HTTP
wiki routes must use the same page repository port.

### F4. Acceptance criteria

- unchanged pages are not regenerated;
- a page lookup is independent of total page count after index warm-up;
- readers never observe a mixed manifest/page version;
- old versions are bounded and safely evicted;
- MCP and HTTP return the same page content and provenance.

## 12. Workstream G: Multi-Service Soak and Scale Validation

### G1. Fixture matrix

Add deterministic fixtures for:

- one 500k-node/1M-edge repository;
- ten repositories with mixed sizes;
- fifty registry entries with smaller artifacts;
- concurrent cold loads for distinct repositories;
- concurrent warm graph, search, wiki, and resource requests;
- index submissions while read traffic is active;
- change sets over 1,000 symbols.

### G2. Scenario runner

Add a repeatable release/performance command that records:

- request throughput and p50/p95/p99 latency;
- event-loop delay;
- RSS and retained cache bytes over time;
- blocking and index queue depth;
- cache hit/miss/eviction/oversize counts;
- error and partial-result counts;
- index job completion and cancellation outcomes.

### G3. Soak policy

- run for at least 30 minutes in CI or a scheduled workflow;
- use bounded fixture directories and cleanup policy;
- fail on monotonic retained-memory growth after cache TTL/eviction windows;
- fail if queue depth or active jobs exceed configured limits;
- fail if p95/p99 regress beyond documented tolerances.

### G4. Acceptance criteria

- ten-service and fifty-repository scenarios remain stable;
- cache state remains bounded after repeated version churn;
- no request path blocks the Tokio event loop beyond the target;
- cancellation and timeout do not leak child processes or queue entries;
- report is stored with machine/configuration metadata.

## 13. Workstream H: Small Synchronous Reads Policy

The current small synchronous reads are not part of the cold-artifact problem,
but the policy must be explicit.

Choose one of:

1. keep them synchronous and add a documented maximum file/stat cost plus a
   regression test;
2. move them behind a lightweight bounded filesystem executor;
3. cache the sidecar/stat metadata by artifact version and invalidate on
   change.

Preferred first step: measure on local and network filesystems, then use option
3 for repeated overview/status requests if needed. Avoid wrapping every
microsecond stat in `spawn_blocking` without evidence.

Acceptance criteria:

- policy is documented in architecture and operations docs;
- tests cover missing, stale, and changed sidecars;
- event-loop delay remains below the scale target during repeated overview and
  status calls.

## 14. Workstream I: Process Runner Boundary

### I1. Extract the port

Move process lifecycle behavior from `local_job_scheduler.rs` into:

- `ports/process_runner.rs` for the contract and typed outcome;
- `infrastructure/engine_process_runner.rs` for Tokio process execution.

The scheduler should own admission, job state, deduplication, and invalidation;
the runner should own process I/O, environment allowlisting, deadlines, and
termination.

### I2. Acceptance criteria

- scheduler tests use a fake runner and no real child process;
- process-runner contract tests cover success, non-zero exit, timeout,
  cancellation, launch failure, output truncation, and allowlisted environment;
- the production runner preserves current behavior;
- process tests remain platform-aware and do not assume Unix-only process
  groups on unsupported platforms.

## 15. Execution Order

Implement in this order:

1. Observability event and queue metrics.
2. Uniform completeness metadata.
3. Process runner port extraction.
4. BM25 profiling and optimization.
5. Persisted artifact-index format and in-memory fallback.
6. Cross-repository strategy evaluation and port.
7. Page-level wiki materialization.
8. Multi-service soak runner and scheduled workflow.
9. Small synchronous-read policy and final documentation alignment.

This order is intentional: metrics and completeness are prerequisites for
interpreting every later benchmark; process-runner extraction reduces test cost;
search and artifact representations are measured before changing storage;
wiki and cross-repository strategies are evaluated against real memory data;
soak tests run only after the metrics they need exist.

## 16. Commit Slices

Each slice must remain independently buildable:

1. `feat(server): add request completion events and runtime gauges`
2. `feat(server): add bounded completeness metadata to graph outputs`
3. `refactor(server): extract engine process runner port`
4. `perf(server): profile and optimize 500k document search`
5. `feat(server): persist artifact indexes with safe fallback`
6. `feat(server): add cross-repository traversal strategy port`
7. `feat(server): materialize wiki pages incrementally`
8. `test(server): add multi-service soak harness`
9. `docs(server): close operational and performance documentation gaps`

Avoid combining representation changes, response-contract changes, and
observability changes in one commit.

## 17. Validation Gates

Every slice:

```bash
cargo fmt --all --check
cargo clippy -p cih-server --all-targets -- -D warnings
cargo test -p cih-server --all-targets
```

Shared infrastructure or public compatibility changes:

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Scale-sensitive slices additionally run:

```bash
cargo run --release -p cih-server --example scale_bench -- \
  --nodes 500000 --edges-per-node 2 --iterations 20 --enforce
```

The report must include machine, Rust/profile, configuration, fixture, cache,
queue, latency, RSS, and acceptance data. A benchmark with a failed acceptance
item is evidence, not a passing performance gate.

## 18. Definition of Gap Closure

The gap-closure plan is complete when:

- all currently open items in the parent plan are either implemented or
  explicitly re-baselined with approval and measured evidence;
- the 500k benchmark passes or the target is formally revised;
- artifact/search memory behavior is bounded and documented for oversize
  repositories;
- persisted or memory-mapped artifact indexes are available with safe fallback;
- cross-repository strategy selection is measured and bounded;
- wiki pages are incrementally materialized and atomically published;
- the multi-service soak test runs on a schedule and detects memory/queue
  regressions;
- one structured completion event exists for every request;
- blocking and index queue counters are observable;
- all bounded application outputs expose honest completeness metadata;
- process execution is behind a tested `EngineProcessRunner` port;
- small synchronous-read policy is documented and tested;
- parent plan status and performance reports match actual implementation state.

## 19. Implementation Checklist

- [x] structured per-request completion event;
- [x] blocking-lane active/queued/rejected metrics;
- [x] index-queue depth/running/rejected metrics;
- [x] completeness metadata for bounded graph outputs;
- [x] separate `EngineProcessRunner` port and adapter;
- [x] BM25 p95 under the 500 ms target (16.2-17.1 ms measured);
- [x] persisted artifact indexes with safe fallback;
- [x] cross-repository strategy port and GraphStore capability evaluation;
- [x] page-level wiki materialization and guarded atomic publication;
- [x] scheduled ten/fifty-repository soak test;
- [x] explicit synchronous-read policy enforced by the blocking runtime;
- [x] parent plan and performance report updated.

### Residual measured limit

The persisted format closes rebuild latency and restart reuse, not resident
memory. The 500k fixture still estimates the parsed artifact snapshot at about
762 MiB and the search index at about 362 MiB, so both exceed default cache
budgets and use oversize bypass. Compact ordinal storage or memory mapping is a
separate representation project with its own compatibility and benchmark gate.
