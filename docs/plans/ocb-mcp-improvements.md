# OCB MCP Improvement Plan (6 phases)

## Context

Tracing `POST /rest/v1/customerservices/change-password` on the 500k-node OCB graph surfaced 6 issue areas (user report, priority-ordered). Exploration confirmed the root causes — several "missing features" actually exist but are disconnected:

| Report item | Verified root cause |
|---|---|
| 1. False self-recursion instead of `UserImpl` | `@Qualifier` never parsed; XML `BeanDef.id` parsed but `#[allow(dead_code)]` (`di_xml.rs:31`); `di_redirect` fallback `single_programmatic_impl` silently picks the sole in-scope implementor — the calling class itself — at conf 0.9 (`emit.rs:416-439`, drift from ROADMAP.md:643 "no silent wrong-impl guess") |
| 2. `INSERT INTO AUDIT_LOG` invisible | SQL machinery exists (`DbQuery`/`DbTable`, `WRITES_TABLE`) but execution-site detection is hardcoded to `DBUtil`/`JdbcTemplate` receivers (`cih-lang .../constants.rs:241-294`) — misses the audit queue; and `trace_flow` BFS never traverses DB edges |
| 3. Trace too broad, truncated | Hardcoded `LIMIT 100` in both adapters (`cih-falkor/query.rs:681`, `cih-ladybug/query.rs:769`); no filters; completeness always reports `complete:true` (`completeness.rs:57`) |
| 4. "blocking runtime / graph store unavailable", 1–2 min greps | grep default concurrency **1** process-wide + `grep_dir` walks the entire tree even for a single-file glob (glob is a filter, not a prune); Falkor **reads** don't retry `BusyLoadingError` (writes do) |
| 5. `routes: 0`, line ranges 0 | Route count only filled by `discover`, which MCP `index_repo` never runs (`local_job_scheduler.rs:498-502`); ranges persisted to graph but every read query omits them and `node_from_row` hardcodes `Range::default()` (both adapters) |
| 6. SQL search buried, no path query | `DbQuery` not a searchable kind, SQL text never enters BM25; no reachability tool |

Pipeline fact driving Phase 1's design: `analyze` runs `resolve_with_registry` (emit-time `di_redirect`, `analyze/mod.rs:476`) **before** `run_graph_augmentation` (`:508`) where DI XML is parsed today — the bean-id map must be collected before resolve.

Phase order = user's priority order. Each phase lands independently on `dev`. **Re-index needed: Phases 1, 2a/2b, 3c** (bundle into ONE OCB re-index via the Windows runbook); everything else works on existing graphs.

---

## Phase 1 — DI / dispatch correctness (re-index: YES)

**1a. Parse `@Qualifier` / `@Resource(name=)` onto bindings**
- `cih-core/src/ir.rs` `TypeBinding`: add `pub qualifier: Option<String>` (serde default/skip-none); mechanical struct-literal fixups workspace-wide.
- `cih-lang/src/java/parse/references.rs::type_binding()`: for Field (and Param, constructor injection) bindings, extract qualifier via existing helpers `annotations()` / `annotation_name()` / `annotation_string_values(node, src, &["value"|"name"])`.
- Bump `PARSE_CACHE_SCHEMA` 25→26 (`cih-lang/src/lib.rs:34`) + paired GOLDEN hash in `cih-engine/tests/parse_schema_guard.rs`.

**1b. Bean-id→FQCN plumbing (hoist DI-XML collection before resolve)**
- `cih-resolve/src/di_xml.rs`: new `pub struct DiWiring { beans, references, beans_by_id: HashMap<String,String> }` + `pub fn collect_di_wiring(repo_root) -> DiWiring` (un-deads `BeanDef.id`). Split `extract_di_xml` into `extract_di_xml_from(&DiWiring, &[ParsedFile])` + keep the walking wrapper (byte-identical when absent).
- `cih-resolve/src/lib.rs`: `ResolveOptions.di_wiring: Option<&DiWiring>` → `index.set_di_wiring(w)`.
- `cih-resolve/src/index.rs`: `di_beans_by_id` map + `bean_class_by_id()`; `field_qualifiers: HashMap<(class_fqcn, field_name), String>` built from field TypeBindings + `field_qualifier()`.
- `cih-resolve/src/augment.rs`: `AugmentCtx.di_wiring`; `DiXmlAugmentor` uses it when present (no double tree walk), else current path.
- `cih-engine/src/analyze/mod.rs` (~:472): collect wiring once when Java in scope, pass to both `ResolveOptions` and `AugmentCtx`.

**1c. Qualifier-aware redirect chain + self-redirect guard**
- Trait `di_redirect` evolves (default `None`; only Java overrides): takes `DiSite { qualifier, enclosing_class }`, returns `DiRedirect { target, confidence, reason }`.
- Java chain, every strategy skipping candidates equal to `enclosing_class`:
  1. qualifier → `bean_class_by_id`, accept iff known type whose supertype closure contains the interface → **0.95, `di-qualifier`**
  2. `single_bean_impl` → 0.9, `di-resolved` (unchanged)
  3. `single_programmatic_impl` → **demoted to 0.75, new reason `di-single-impl`**
- `emit.rs::resolve_receiver_bound_call`: look up field qualifier for simple-identifier receivers; when all strategies exhausted and the interface method exists, emit a truthful **interface-method edge** (0.7, `receiver-bound`) — the OCB case degrades honestly instead of fabricating recursion. No generic `src==dst` guard in `push_edge` (real recursion is legitimate); the guard lives in redirect selection. Add strategy-map entries: `di-qualifier`→`di_bean_id`, `di-single-impl`→`iface_single`.
- **Deferred (recommend)**: emitting all candidate impls on ambiguity — would bloat the 1.5M-edge graph with noise Phase 3 is trying to remove; interface edge + `di-xml-*` class edges preserve discoverability.

**Tests**: `cih-resolve/tests/resolve.rs` (mirror existing DI tests ~:781/:789/:939): `qualifier_redirect_follows_xml_bean_id`, `programmatic_fallback_never_targets_enclosing_class` (OCB regression), demoted-confidence check. `tests/di_xml.rs`: `beans_by_id`. `cih-lang/tests/java.rs`: qualifier capture. New corpus fixture `cih-engine/tests/corpus/java-spring-xml-di/` (interface + 2 impls + `META-INF/spring` XML with bean ids + `@Qualifier` field) gated by a `di_corpus.rs` mirroring `aop_corpus.rs`; report before/after A/B resolved-edge counts per CLAUDE.md.

## Phase 2 — DB + async side effects (re-index: 2a/2b YES; 2c server-only)

**2a. Widen execution-site detection** (`cih-lang/.../constants.rs`)
1. Config-free fallback: accept any `method_invocation` whose identifier argument names a known SQL constant in scope → `SqlExecutionSite` with new `heuristic: bool` IR flag (serde default false), tagged into `DbQuery` props by `db_access.rs`. This catches OCB's audit queue without config.
2. Config-driven API set: `cih.toml` `[analyze.sql] extra_apis = ["AuditQueue.enqueue", ...]` → `JavaProvider::with_sql_apis(...)` via the documented persisted-option recipe (`cmd/args.rs` + `settings.rs`).
   **Mandatory**: mix a config fingerprint into the parse-cache namespace (`.cih/parse-cache/v26-<cfghash>/`) — parse output now depends on config; without this, changed config silently serves stale IR.

**2b. Relax SQL-constant capture**: accept any `static final String` whose folded value starts (case-insensitive) with `SELECT|INSERT|UPDATE|DELETE|MERGE|CALL|WITH` (OR'd with current UPPER_SNAKE rule — strict superset).

**2c. `db_effects` summary on traces** (server-only, works on current graphs). Summary section, NOT BFS edge inclusion (DbQuery/DbTable aren't callables; hops would distort depth and add the noise Phase 3 removes):
- `cih-graph-store` trait: `async fn db_effects_for_methods(&self, ids: &[NodeId]) -> Result<Vec<DbEffect>>` where `DbEffect { method, query, operation, table, access READ|WRITE, sql_preview }`.
- Falkor + Ladybug: `MATCH (m) WHERE m.id IN [...] MATCH (m)-[:EXECUTES_QUERY]->(q)-[r:READS_TABLE|WRITES_TABLE]->(t) ...`.
- `application/graph/mod.rs::trace_flow`: after hops, collect method ids → attach `db_effects` (skip-if-empty) to `TraceFlowOutput`. Async boundary beyond `PUBLISHES_EVENT|LISTENS_TO`: consciously deferred (2a config + Phase 6 `reaches` cover the OCB path).

**Tests**: `cih-resolve/tests/db_access.rs` (heuristic tagging), `cih-lang/tests/java.rs`, graph-store contract suite case for `db_effects_for_methods`.

## Phase 3 — Filterable, paginated traces (re-index: only 3c)

**3a. Filter/paging through all 4 layers**
- `TraceFlowArgs` (`args.rs:222`): `exclude_kinds: Vec<String>`, `business_only: bool` (= exclude accessors+constructors), `max_nodes` (default 100, clamp 1..=500), `offset`.
- Trait change (`cih-graph-store/src/lib.rs:295`): `flow_downstream(&self, entry, filter: &FlowFilter) -> Result<FlowPage>` with `FlowFilter { max_depth, exclude_kinds, exclude_accessors, limit, offset }`, `FlowPage { hops, has_more }`.
- Both adapters: add kind/accessor WHERE clauses (`coalesce(m.isAccessor,false)` — no-op on pre-fix graphs), replace `LIMIT 100` with `SKIP {offset} LIMIT {limit+1}` (+1 row = honest `has_more` probe). Route-entry branch forwards the filter to handler sub-walks.
- **Atomic parity commit**: trait + falkor + ladybug + contract suite (`contract.rs` flow section ~:577) + every `flow_downstream(` caller (incl. cross-repo `trace_flow_x`).

**3b. Honest completeness + continuation**: new `ResultBounds::paged(returned, offset, has_more, limit)`; swap in **only** trace_flow's call site (`graph/mod.rs:146` — `requested_scope` has other callers, leave them). `TraceFlowOutput.next_offset: Option<u32>`. Mermaid renderer appends a truncation note.

**3c. `is_accessor` node prop** (index-time, needs re-index; degrades to no-op without): computed in Java parser by porting `cih-embed/src/strip.rs::is_trivial_getter_body` into a shared helper; persisted as a first-class column in both adapters' serialize/bulk-load paths (follow the promoted-complexity precedent).

**Tests**: `cih-server/tests/args.rs` defaults; contract suite: exclusion honored, page determinism (page1+page2 == unpaged prefix), `has_more`, accessor exclusion (add accessor Method to fixture).

## Phase 4 — Transient failures + grep perf (re-index: NO)

- **4a. Grep literal fast path** (`application/files/mod.rs`): new pure fn `literal_walk_prefix(glob) -> Option<PathBuf>` (deepest metacharacter-free prefix). Whole-glob-literal → stat + scan that file directly (O(1) instead of walking 500k files); literal dir prefix → root the `WalkBuilder` at the subtree (keep paths relative to repo_root). Same containment validation as `read_file`; still inside `GrepRuntime`.
- **4b.** `CIH_GREP_MAX_CONCURRENT_REQUESTS` default 1 → 2.
- **4c. Loading-aware Falkor reads** (`cih-falkor/src/lib.rs`): new `run_read` — on `is_loading_error`, `wait_until_ready` with bounded budget (`CIH_FALKOR_READ_LOAD_WAIT_SECS`, default ~20s vs writes' 600s) then retry once; switch `rows()` to it. Removes intermittent "graph store unavailable" after restarts.
- **4d. Error copy**: `Saturated` grep message names the knob; per-call-site `dependency` labels ("grep", "file read") so saturation stops reading as "blocking runtime unavailable".
- **4e. architecture_overview partial degradation**: wiki path is already graceful (`wiki_warning`); extend the same `available:false` + remedy convention to per-section compose graph queries; repo-resolution stays the only hard failure. Scope tightly.

**Tests**: extend inline grep tests in `files/mod.rs` (`test_grep` ~:954, `grep_glob_filters_files` ~:998): literal fast path, subtree prune, containment rejection; unit tests for `literal_walk_prefix` and the read-retry predicate helpers.

## Phase 5 — Metadata consistency (re-index: NO; one re-analyze refreshes route counts)

- **5a. routes:0**: analyze fills it — `EmitOutcome.route_node_count` (count `NodeKind::Route` at node assembly), `entry_from_analyze` sets `stats.routes`, extend the reused-artifacts carry-forward (as done for `resolved_edges`); `update_entry_from_discover` still overwrites with its richer count. `status` renders a "stale — re-run analyze" marker for pre-fix entries instead of ambiguous 0.
- **5b. Line ranges 0**: define one node-column-list constant per adapter including `startLine, endLine`; update every query feeding `node_from_row` (falkor `query.rs:108` + neighbor/candidate reads; ladybug `convert.rs:77-87` has the identical gap); populate `Range`. No re-index — data is already in the graph. Contract suite: assert non-zero ranges on `symbol`, `candidates_by_name`, `nodes_in_files`.

## Phase 6 — SQL search + `reaches` tool (re-index: NO; 6a needs sidecar rebuild via re-analyze)

- **6a.** `cih-search`: add `NodeKind::DbQuery` to `is_searchable_kind`; `collect_node_tokens` DbQuery branch (tokens: `tables[]`, `operation`, `constantName`, head of `sqlPreview`). Bump sidecar version constant so stale sidecars regenerate. Ranking: rely on BM25 idf first; touch RRF merge only if Fineract evaluation shows burying.
- **6b. New `reaches` MCP tool** ("does confirmChangePassword reach a write to AUDIT_LOG?"):
  - Trait: `paths_between(from, to, max_depth, max_paths) -> Vec<PathInfo>` (`PathInfo { nodes, edges: Vec<PathEdge{kind,reason,confidence}>, min_confidence }`).
  - Adapters: variable-length match over `CALLS|EXECUTES_QUERY|READS_TABLE|WRITES_TABLE|HANDLES_ROUTE|PUBLISHES_EVENT|LISTENS_TO`, `ORDER BY length(p) LIMIT max_paths` (edge confidence/reason are persisted — verified `serialize.rs:86-90`).
  - `ReachesArgs { from, to, max_depth default 8 clamp 1..=12, max_paths default 3 clamp 1..=10, repo }`; `to` resolves via existing symbol disambiguation with a bare-name → `DbTable:<NAME>` fallback.
  - Register in `transport/mcp/tools/graph.rs`; update CLAUDE.md tool table (31 → 32 tools) + `docs/agent-workflows/`.

---

## Sequencing & verification

1. Land in phase order. Phases 1 + 2a/2b + 3c share the `PARSE_CACHE_SCHEMA` bump window → **one** OCB re-index on the Windows laptop; update `docs/runbooks/` after Phase 3.
2. Per phase: `cargo test --workspace` (hermetic), clippy `-D warnings`, fmt, `architecture_boundaries.rs`. Corpus A/B resolved-edge/unresolved-ref numbers for parser/resolver changes (raise floors in `corpus_coverage.rs` if improved). Ladybug contract hermetic + Falkor via `cargo test -p cih-falkor --test falkor_integration -- --ignored` for every trait change.
3. End-to-end on the Fineract stack (graph key `fineract`, port 8081): trace_flow filters/paging, db_effects, reaches, grep fast path timing.
4. Final OCB verification: re-run the change-password trace — expect `CustomUserImpl.modifyUserPassword → UserImpl.modifyUserPassword` (`di-qualifier`, 0.95) → `AuditLogServiceImpl.audit`, with `db_effects` showing `WRITE AUDIT_LOG`, and `reaches(confirmChangePassword, AUDIT_LOG)` returning a confident path.

## Risk flags

- **Qualifier target only in a JAR** (not parsed source): strategy 1 requires a known type → falls through to the honest interface edge. Correct, but decompile-enabled runs resolve more; document in runbook.
- **Parse-cache invalidation for config-driven SQL APIs (2a)** is the easiest silent failure — the config fingerprint in the cache namespace is mandatory.
- **`FlowFilter` trait change** must land atomically across both adapters + contract + all callers.
- **`requested_scope`** has other callers (e.g. impact) — change only trace_flow's call site.
