# yummy-cih — Implementation Roadmap

Phased build plan for **CIH (Code Intelligence Hub)**. Architecture rationale lives in
`../cih-plan.md`; Rust engine internals in `../codegraph-rust-plan.md`; the system diagram in
`../high-architecture`.

**Guiding principle:** ship a *thin vertical slice* first (index a Java repo → query it over MCP),
then make the call graph *accurate*, then add *product*, then *scale to go-live*. Depth before
breadth; one demoable capability per milestone.

**Status:** ✅ done · 🚧 in progress · ⬜ not started.
**Critical path to value:** Phase 1 → 2 → 3 → 4. Everything after enriches, productizes, or scales.

---

## Phase 0 — Foundations ✅ (scaffold done)

- **Goal:** workspace + ports + a runnable shape to build against.
- **Built:** Cargo workspace; `cih-core` (Node/Edge/NodeId/Kind/Range/GraphArtifacts);
  `cih-graph-store` (the `GraphStore` + `BulkLoader` ports + domain types); `cih-falkor`
  (FalkorDB read adapter, real openCypher); `cih-server` (rmcp + axum skeleton, tools
  `context`/`impact`); README.
- **Done when:** crate tree is structurally complete (compile pending Phase 1).

## Phase 1 — MCP service runs end-to-end (read path) ✅

- **Goal:** a running MCP server callable from MCP Inspector against FalkorDB.
- **Build:** pin & reconcile `rmcp` until `cargo build` is green; verify `context`/`impact` over
  Streamable HTTP; add a `query` tool stub; structured tracing; `docker run falkordb` dev loop.
- **Done when:** hand-seed a graph in FalkorDB → Inspector calls `impact`/`context` → typed results.
  🎯 **Milestone: live MCP endpoint.**
- **VERIFIED 2026-06-13:** built on `rmcp 0.7.0` (fixes: `wrapper::Parameters`,
  `Implementation { ..Default::default() }`, `schemars = "1"`). `tools/call impact` returns the
  upstream caller end-to-end (MCP → FalkorStore → FalkorDB → typed `Impact`).
  **Dev gotcha:** a Homebrew `redis-server` squats on `127.0.0.1:6379`, so run FalkorDB on **6380**
  (`docker run -d --name falkordb -p 6380:6379 falkordb/falkordb`) and start the server with
  `FALKOR_URL=redis://127.0.0.1:6380`.

## Phase 2 — Graph write path (BulkLoader + incremental) ✅

- **Goal:** load a real graph programmatically, not by hand.
- **Build:** finalize the canonical `GraphArtifacts` schema (nodes/edges parquet+csv) in `cih-core`;
  `FalkorBulkLoader` (UNWIND batches + index creation); implement `bulk_load` /
  `upsert_incremental` / `publish_to` on `FalkorStore`; **switch queries to FalkorDB query
  parameters** (drop the stub inline-escaping).
- **Done when:** load a synthetic ~10k-node graph; queries return; re-load is idempotent.
- **VERIFIED 2026-06-13:** canonical artifacts are **JSONL** (`nodes.jsonl`/`edges.jsonl`) in
  `cih-core` (`GraphArtifacts::write`/`read_nodes`/`read_edges`) — Parquet deferred to the Neptune
  S3 path (Phase 11). `FalkorStore::{bulk_load,upsert_incremental}` + `FalkorBulkLoader`
  implemented (UNWIND-batch MERGE, idempotent; edges grouped per type; `upsert` =
  delete-by-file then re-load). Read queries now use FalkorDB `CYPHER` parameters.
  Smoke test: `cargo run -p cih-falkor --example load_sample` → impact returns the full call chain.
  Remaining polish: Parquet + S3 (Phase 11), compact-protocol list parsing for `call_chain`.

## Phase 3 — Engine MVP: scan → scope → parse → structure ✅  🎯 first vertical slice

- **Crates:** `cih-parse`, `cih-lang` (JavaProvider), `cih-engine` (orchestrator), `cih-jar`;
  extend `cih-core` IR.
- **Build:** file scan with ignore rules; tree-sitter-java parse via the scope query
  (port `languages/java/query.ts`); extract `Class`/`Interface`/`Method`/`Field` defs + `File`/
  `Folder` nodes + `CONTAINS`/`HAS_METHOD` edges → emit `GraphArtifacts`; `rayon` parallel parse;
  wire engine → `BulkLoader` → FalkorDB.
- **Done when:** `cih-engine analyze <java-repo>` loads real symbols; MCP `context` shows them.
  (Calls still absent/crude — that's Phase 4.) 🎯 **Milestone: index → query a real repo.**
- **VERIFIED 2026-06-14:** delivered as 8 tasks (detail in `docs/phase-3.md` + `phase-3-impl-spec.md`),
  refined with a **scan→scope-first** flow so a 12k-file repo isn't all-or-nothing:
  - **scan** (`cih-engine scan`) — parse-free walk → `RepoMap` (modules, LOC, Spring counts) +
    `.cih/repo-map.json` + recommendation; **scope** (`analyze --all|--module|--include|--exclude` or
    `cih.scope.toml`) → resolved file list → `.cih/scope.json` (module-subtree + name-collision aware).
  - **parse** (`cih-parse`, rayon, thread-local parser) → structure nodes/edges +
    `ParsedFile` IR (defs, imports, **unresolved `ReferenceSite`s** for Phase 4) → `parsed-files.jsonl`;
    robust skip-and-count on bad files.
  - **emit + load** — content-hash `VersionId` → `GraphArtifacts` JSONL → `FalkorStore::bulk_load`;
    stale-version pruning; exit-3 on DB-load failure; idempotent.
  - **Spring tags (Task 7, pulls Phase 7 forward):** per-class `stereotype` prop from the class's own
    annotations (controller/service/repository/component/configuration/entity/resource) + `Route`
    nodes + `HANDLES_ROUTE` edges (class `@RequestMapping` prefix joined with method `@*Mapping`).
  - **JAR API-surface (Task 8, the source-less-lib unlock, Phase-8 API part):** `cih-jar` reads
    `.class` via `cafebabe` (no JDK/decompiler) → signature-only Class/Method/Constructor/Field nodes
    with **locked ids**, demand-driven `include` filter. Engine wiring waits on Phase 4's
    unresolved-ref set.
  - Verified live on FalkorDB :6380; workspace clippy clean, all crate tests green.

## Phase 4 — Scope resolution + MRO ✅  🎯 accurate call graph

- **Plan:** `docs/phase-4.md`.
- **Crate:** `cih-resolve`.
- **VERIFIED 2026-06-14:** full resolution pipeline delivered in 5 sub-phases:
  - **4.0 IR extension** (`cih-core` + `cih-parse`): `TypeBinding { kind: BindingKind, .. }`,
    `SymbolDef.param_types/return_type/declared_type`, `ReferenceSite.in_callable: NodeId`;
    `cih-parse` persists type bindings, param/return types, and caller ids.
  - **4.1 `ResolveIndex`** (`cih-resolve`): def/type/heritage/import indexes; precedence-ordered
    scope-binding lookup (Param > Local > Pattern > Field > CallResult > Alias > Return + range
    proximity for shadowing); `find_member_in_hierarchy` (BFS with arity cascade).
  - **4.2 Emit passes** (ordered, per-site dedup): receiver-bound 7-case dispatcher →
    free-call fallback → references-via-lookup (Ctor/FieldRead/FieldWrite/TypeRef) →
    import edges → heritage; `edge.src = site.in_callable`; `skipped` counter + external FQCN set.
  - **4.3 C3 MRO**: `c3_linearize` (memoized, cycle-safe); `build_mro_map` over all scope types;
    `emit_mro_edges` — one `METHOD_OVERRIDES` to nearest class ancestor; all
    `METHOD_IMPLEMENTS` to interface ancestors. Fixed `stable_dedup` so superclass-first
    heritage order is preserved for C3.
  - **4.5 Versioning + wiring**: `content_version` covers structure nodes + combined
    (structure+resolved) edges + `ParsedFile` IR — IR-only body changes bump the version;
    `cih-engine resolve <repo>` subcommand reads saved `.cih/scope.json` and re-runs
    resolution without rescanning; `load_parsed_files` in `cih-parse` for offline IR loading.
  - Workspace: 43 tests green, clippy clean; `combined_edges` deduplicates on (src, dst, kind)
    keeping highest confidence.
- **4.4 ✅ VERIFIED 2026-06-14** (delivered post-milestone, closes the Phase-8 wiring gap):
  - **4.4a JAR discovery** (`cih-engine/scan/jars.rs`): `discover_jars` catalogs JARs from
    `lib/`/`libs/`/`.workspace-dependencies/` (local walk), Maven `~/.m2/repository/` (targeted
    per dep), and Gradle `~/.gradle/caches/modules-*/files-*/` (targeted per dep) into
    `RepoMap.jars`. Counts `.class` entries, extracts group/artifact from Maven/Gradle path
    layouts, marks own-vs-third-party via `own_group_prefix`. 7 tests.
  - **4.4b demand-driven JAR API extraction** (`cih-engine/main.rs`): `extract_jar_api` feeds
    `resolve_output.unresolved_external_fqcns` into `JarApiExtractor::with_include(...)` over
    all cataloged JARs; merges resulting nodes+edges into `GraphArtifacts`; JAR nodes/edges are
    included in the content version. `run_resolve` reads `repo-map.json` for jars. 3 new tests
    including end-to-end integration via `cih-jar` sample fixture.
  - Workspace: **57 tests** green, clippy clean.

## Phase 5 — Communities + processes ✅

- **Plan:** `docs/phase-5.md`.
- **VERIFIED 2026-06-14:** Leiden-style community detection + BFS process tracing delivered:
  - **New crate `cih-community`:** `prng.rs` (Mulberry32, seed `0xc0de` for reproducibility),
    `graph.rs` (undirected community graph + directed calls digraph via `petgraph = "0.6"`),
    `leiden.rs` (Louvain Phase 1 local-moving, 60-second timeout + graceful degradation),
    `label.rs` (three-tier heuristic: folder-mode → name-prefix → `Cluster_N`), `cohesion.rs`
    (sampled internal-edge density), `entry_points.rs` (callee/caller ratio × name multipliers,
    top-200 cap), `bfs.rs` (BFS cycle-safe + two-pass dedup: substring-subset removal then
    endpoint-longest). **7 unit tests.**
  - **`cih-engine discover` subcommand:** reads latest `.cih/artifacts/<v>/` (mtime-ranked),
    runs `detect_communities` (resolution 1.0/2.0 auto-selected at 10 001 nodes) +
    `trace_processes` (depth 10, branching 4, dynamic max), writes `Community`/`Process` nodes
    + `MEMBER_OF`/`STEP_IN_PROCESS` edges to `.cih/artifacts-community/<v>/`; prunes stale
    versions; loads to FalkorDB (exit-3 on failure). **1 integration test.**
  - **`cih-falkor context()`:** STEP_IN_PROCESS query now populates `SymbolContext.processes`.
  - **`cih-server communities` tool:** MCP tool lists all detected clusters with cohesion scores.
  - **`cih-core`:** `community_id(idx)` + `process_id(slug, hash)` id helpers.
  - Workspace: **62 tests** green, clippy clean.

## Phase 6 — Search: BM25 + embeddings + hybrid ✅

- **Plan:** `docs/phase-6.md`.
- **VERIFIED 2026-06-14:** hybrid search delivered:
  - **New crate `cih-search`:** tokenizer with punctuation/camel splitting, BM25 over graph symbol
    nodes (`Class`/`Interface`/`Enum`/`Record`/`Annotation`/`Method`/`Constructor`/`Field`/
    `Route`), and Reciprocal Rank Fusion (`k=60`). **5 unit tests.**
  - **New crate `cih-embed`:** character chunker (4 KB / 500 B overlap), deterministic
    `blake3` content hashes, fastembed model wrapper (`all-minilm-l6-v2` default,
    `bge-small-en-v1.5` supported), pgvector table/index DDL, content-hash skip logic, HNSW
    query with exact-scan fallback for small datasets. **3 pure unit tests.**
  - **`cih-engine embed` subcommand:** reads latest `.cih/artifacts/<v>/nodes.jsonl`, ensures
    pgvector schema, embeds eligible graph nodes, and prints human or JSON summary.
  - **`cih-server query` MCP tool:** lazily builds in-memory BM25 from `CIH_ARTIFACTS_DIR`, uses
    optional semantic search from `CIH_PG_URL`, merges both with RRF, and supports `expand=true`
    via `GraphStore::subgraph(top_5, 1)`.
  - Workspace: **70 tests** green. `cargo fmt --check` still reports pre-existing formatting
    diffs in unrelated files (`cih-engine` scan/scope helpers, `cih-falkor`, `cih-jar`,
    `cih-parse`, `cih-resolve`); Phase 6 touched files were rustfmt-formatted.
- **Architecture cleanup 2026-06-14** (`docs/architecture-improvements.md`):
  - `NodeKind::label()` / `from_label()` consolidated in `cih-core`; 3 duplicate copies removed.
  - `ResolveIndex` and all methods sealed to `pub(crate)`; only `resolve_edges()` remains public.
  - `cih-engine/src/main.rs` split from 1 405 lines into 5 focused modules: `analyze`, `discover`,
    `embed`, `db`, `versioning` + `tests`; `main.rs` reduced to **183 lines**.
  - `cih-server/src/search.rs` extracted: `QueryArgs`, `query_hits()`, RRF orchestration.
  - BFS cycle detection upgraded from O(n) `Vec::contains` to O(1) `HashSet`.
  - Added 7 tests (cih-core round-trip, 3 × cih-falkor, 3 × cih-server).
  - Workspace: **77 tests** green, clippy clean.

## Phase 7 — Spring pre-phase ✅ (2026-06-14)

- **Crate:** `cih-spring` (currently inline in `cih-parse`; extract if it grows).
- **Done in Phase 3:** per-class `stereotype` tags (`@Service`/`@Repository`/`@Controller`/
  `@RestController`/`@Configuration`/`@Component`/`@Entity`) from own annotations; routes
  (`@RequestMapping`/`@GetMapping`/… → `Route` + `HANDLES_ROUTE`, class-prefix joined).
- **Completed 2026-06-14** (`docs/phase-7.md`):
  - **`@Bean` detection** — `is_bean_method()` in `cih-parse/src/java.rs` sets `props.isBean=true`
    on Method nodes annotated with `@Bean`; reuses existing `annotations()` helper.
  - **JPA repository tagging** — `jpa_repository_props()` walks the `implements` clause for 10 known
    Spring Data interfaces (`JpaRepository`, `CrudRepository`, `MongoRepository`, …); sets
    `stereotype="repository"` and `entityType=<first generic arg>` on Class nodes; no import
    resolution needed (short names are globally unique by Spring Data convention).
  - **`route_map` MCP tool** — `RouteInfo` struct in `cih-graph-store`; FalkorDB Cypher impl in
    `cih-falkor`; `route_map(prefix, limit)` MCP tool in `cih-server`; path-prefix filter + max
    limit (default 200).
  - 8 new tests (5 cih-parse, 2 cih-falkor, 1 cih-server). Workspace: **85 tests** green, clippy clean.
- **Done when:** routes + beans are queryable; a `route_map` view works on a Spring app. ✅

## Phase 8 — Dependency libs: API-surface ✅ (wiring done in Phase 4.4) + full decompile

- **Done in Phase 3 (`cih-jar`):** signature-only **API-surface** extraction from source-less JARs
  via `cafebabe` (no JDK/decompiler) — Class/Method/Constructor/Field nodes with locked ids,
  demand-driven `include` filter. The high-value path for the 26k own libs.
- **Done in Phase 4.4 (wiring):** JAR catalog (`RepoMap.jars`) + `extract_jar_api` feeds the
  unresolved-reference FQCN set to `JarApiExtractor::with_include(...)` and routes output through
  `GraphArtifacts`/`bulk_load`; app→lib calls now land on the lib's API node instead of dropping.
- **Remaining (full decompile):** for the few libs whose *internals* must be traced through, spawn
  Fernflower → `.java` into `EFS:.workspace-dependencies/` → index as first-class source;
  size-skip guard. (Rare exception; API-surface is the default.)
- **Done when:** calls into a dependency resolve (to its API node, or to decompiled source) instead
  of being silently dropped.

## Phase 9 — Incremental re-index + cache + versioning ✅ (2026-06-14)

- **Plan:** `docs/phase-9.md`.
- **Completed 2026-06-14:**
  - **File hash index** — `.cih/file-hashes.json` stores blake3/16 content hashes for scoped files;
    readable files are hashed in parallel with `rayon`.
  - **Content-addressed parse cache** — `.cih/parse-cache/<hash>.json` stores per-file parse units
    (`ParsedFile` IR plus graph nodes/edges), so unchanged files keep route nodes and Spring/JPA
    props without re-running tree-sitter.
  - **Importer BFS expansion** — changed files are expanded through transitive importers up to
    depth 4; resolution still runs across the complete scoped file set.
  - **No-op reuse** — unchanged scope + identical hash set reuses the latest artifacts and skips
    parse, resolve, and DB reload. `analyze --no-cache` forces full parsing for parser upgrades.
  - **Blue-green Falkor publish** — engine loads into `<graph_key>-staging`, then publishes via
    `GRAPH.COPY staging live REPLACE`, and deletes the staging graph.
  - **Parse API support** — `cih-parse::parse_file_units` and `parse_output_from_units` preserve
    existing `parse_files` behavior while enabling cache composition.
  - Workspace: **93 tests** green, clippy clean, docs build clean.
- **Done when:** re-index after editing one file is fast and correct (only the delta re-parses;
  resolution still runs full-scope). ✅

## Phase 10 — Product: orchestration + chat + wiki  🎯 GraphRAG product

- **Build:** Next.js BFF running the **Claude Agent SDK**, consuming CIH's MCP tools (Opus 4.8 deep
  / Sonnet 4.x chat); chat UI; **Docusaurus** wiki auto-rebuilt from communities/processes.
- **Done when:** a chat question → agent traverses the graph via MCP tools → grounded, cited answer;
  wiki renders. 🎯 **Milestone: usable product.**

## Phase 11 — Storage spike + Postgres-CTE + Neptune adapters

- **Build:** `cih-postgres` (recursive-CTE adapter, ~$0 fallback); `cih-neptune` (openCypher over
  HTTPS+SigV4 + S3-CSV bulk loader); run the **3-way traversal benchmark**
  (FalkorDB / Postgres-CTE / Neptune) at depth 2/4/6/8 + bulk-load time.
- **Done when:** spike numbers recorded; per-env backend confirmed (dev FalkorDB, prod Neptune).

## Phase 12 — AWS go-live

- **Build:** deploy the Rust service (ECS/Fargate or EC2), RDS Postgres+pgvector, S3 (artifacts),
  EFS, Neptune; IAM/VPC/secrets; observability (tracing + metrics); multi-tenant isolation;
  backups/DR; flip `CIH_GRAPH_BACKEND=neptune`.
- **Done when:** production indexes the real large repo; banking-grade controls in place.
  🎯 **Milestone: go-live.**

## Phase 13 — Spring DI-aware resolution (deferred differentiator)

- **Build:** bean-wiring pass — resolve `@Autowired`/constructor injection so an interface-typed
  call (`UserService service; service.save()`) routes to the concrete `@Service` impl; augment
  receiver type bindings before the receiver-bound pass.
- **Done when:** interface calls resolve to the impl in `impact`/`call_chain` (the key Spring edge
  over GitNexus's generic resolver).

## Phase 14 — More languages (generic-pipeline payoff)

- **Build:** add `LanguageProvider` impls (Kotlin next, then others) reusing the generic pipeline;
  per-language scope query + MRO strategy only.
- **Done when:** a second language indexes through the unchanged engine.

---

## Sequencing & parallelism

- **Serial critical path:** 1 → 2 → 3 → 4 (gets you an accurate, queryable call graph).
- **Then enrich (can overlap):** 5, 6, 7 once Phase 4's graph is stable; 8 and 9 independently.
- **Product (10)** can start as soon as 4 + 6 give queryable + searchable data.
- **Adapters (11)** can be written anytime after the `GraphStore` port is stable (post-Phase 2) —
  they don't block the engine.
- **13 & 14** are intentionally last (differentiator + breadth), after the core is proven.

## Definition of done (overall v1)

Index the real Java/Spring repo (incl. decompiled deps) → accurate call graph in FalkorDB (dev) /
Neptune (prod) + vectors in pgvector → MCP tools + a Claude-Agent-SDK chat product answer
impact/architecture questions with grounded citations, on banking-grade AWS.
