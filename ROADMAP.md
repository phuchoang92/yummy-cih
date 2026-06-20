# yummy-cih — Implementation Roadmap

Phased build plan for **CIH (Code Intelligence Hub)**. Architecture rationale lives in
`docs/cih-plan.md`; Rust engine internals in `docs/codegraph-rust-plan.md`; the system diagram in
`docs/high-architecture.mmd`.

**Guiding principle:** ship a *thin vertical slice* first (index a Java repo → query it over MCP),
then make the call graph *accurate*, then add *product*, then *scale to go-live*. Depth before
breadth; one demoable capability per milestone.

**What is a multi-persona AI agent system?**
yummy-cih is the AI backend for the **yummy** frontend. A single shared code-intelligence graph
(FalkorDB) is queried by an AI agent (Claude via the Agent SDK) that adapts its answers to the
user's role. The same graph serves four personas — each gets a dedicated set of MCP tools and a
persona-specific chat view in yummy:

| Persona | Core need | CIH answers |
|---------|-----------|-------------|
| **Developer** | Understand unfamiliar code; assess blast radius of a change | `context`, `impact`, `query`, `trace_flow` |
| **PO** (Product Owner) | Know what the system does; estimate effort for incoming CRs | `route_map`, `feature_map`, `cr_impact` |
| **BA** (Business Analyst) | Trace end-to-end business flows; map features to code | `trace_flow`, `cr_impact`, `feature_map` |
| **Tester** | Find what tests cover which code; know what to re-run after a change | `test_coverage`, `regression_scope`, `untested_paths` |

This differs from file-level coding assistants (Claude Code, Kiro): CIH answers *system-level*
questions from a persistent, always-fresh indexed graph — no re-reading thousands of files per
question. The yummy frontend exposes persona-filtered chat views over this single backend.

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
- **VERIFIED 2026-06-14:** delivered as 8 tasks (detail in `docs/plans/phase-3.md` + `phase-3-impl-spec.md`),
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

- **Plan:** `docs/plans/phase-4.md`.
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
  - Workspace: 43 tests green *(at the time)*, clippy clean; `combined_edges` deduplicates on (src, dst, kind)
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
  - Workspace: **57 tests** green *(at the time)*, clippy clean.

## Phase 5 — Communities + processes ✅

- **Plan:** `docs/plans/phase-5.md`.
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
  - Workspace: **62 tests** green *(at the time)*, clippy clean.

## Phase 6 — Search: BM25 + embeddings + hybrid ✅

- **Plan:** `docs/plans/phase-6.md`.
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
  - Workspace: **70 tests** green *(at the time)*. `cargo fmt --check` still reports pre-existing formatting
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
  - Workspace: **77 tests** green *(at the time)*, clippy clean.

## Phase 7 — Spring pre-phase ✅ (2026-06-14)

- **Crate:** `cih-spring` (currently inline in `cih-parse`; extract if it grows).
- **Done in Phase 3:** per-class `stereotype` tags (`@Service`/`@Repository`/`@Controller`/
  `@RestController`/`@Configuration`/`@Component`/`@Entity`) from own annotations; routes
  (`@RequestMapping`/`@GetMapping`/… → `Route` + `HANDLES_ROUTE`, class-prefix joined).
- **Completed 2026-06-14** (`docs/plans/phase-7.md`):
  - **`@Bean` detection** — `is_bean_method()` in `cih-parse/src/java.rs` sets `props.isBean=true`
    on Method nodes annotated with `@Bean`; reuses existing `annotations()` helper.
  - **JPA repository tagging** — `jpa_repository_props()` walks the `implements` clause for 10 known
    Spring Data interfaces (`JpaRepository`, `CrudRepository`, `MongoRepository`, …); sets
    `stereotype="repository"` and `entityType=<first generic arg>` on Class nodes; no import
    resolution needed (short names are globally unique by Spring Data convention).
  - **`route_map` MCP tool** — `RouteInfo` struct in `cih-graph-store`; FalkorDB Cypher impl in
    `cih-falkor`; `route_map(prefix, limit)` MCP tool in `cih-server`; path-prefix filter + max
    limit (default 200).
  - 8 new tests (5 cih-parse, 2 cih-falkor, 1 cih-server). Workspace: **85 tests** green *(at the time)*, clippy clean.
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

- **Plan:** `docs/plans/phase-9.md`.
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
  - Workspace: **93 tests** green *(at the time)*, clippy clean, docs build clean.
- **Done when:** re-index after editing one file is fast and correct (only the delta re-parses;
  resolution still runs full-scope). ✅

## Phase 10 — Product: multi-persona chat + wiki  🎯 usable product

- **Build:** Next.js BFF running the **Claude Agent SDK** (Opus 4.8 deep / Sonnet 4.x chat),
  consuming CIH MCP tools; **persona-specific chat views** in yummy for Developer / PO / BA /
  Tester.
- **MCP tool contract:** tools return structured JSON (not free text) so yummy can render diagrams,
  tables, and source citations alongside chat answers.
- **Done when:** a PO asks "what APIs does the payment module expose?" → agent calls
  `route_map(prefix="/payment")` + `communities()` → cited answer rendered in yummy; wiki renders
  the community graph as navigable docs. 🎯 **Milestone: usable product for all four personas.**
- **Pre-requisites now met:** wiki generation (10a), DB access in pages (10b), Docusaurus viewer
  (`docs-viewer/` with `CIH_WIKI_PATH`), all four persona MCP tools (phases 15–22), agent workflow
  docs (phase 20), and registry (phase 18) are all ✅. Remaining work is the yummy Next.js
  frontend and Claude Agent SDK integration.

## Phase 10a — `cih-engine wiki`: Graph Artifacts to Role-Based Wiki Bundle ✅ (2026-06-16)

- **`cih-engine wiki <repo> [--out <dir>] [--llm] [--llm-provider <...>] [--json]`** — reads
  existing analyze + discover JSONL artifacts, writes a self-contained Markdown + JSON wiki bundle
  for PO, BA, and Dev readers. No FalkorDB, no MCP server — pure file-in, file-out.
- **New crate `cih-wiki`**: `WikiGraph` (19 deterministic BTreeMap indexes), `generate_wiki()`
  orchestration, **feature-first page hierarchy** (`pages/<feature>/po.md`, `ba.md`,
  `dev/<class-slug>.md`), OpenAPI + D3-force sidecars, `manifest.json` (schema v1),
  Docusaurus-compatible frontmatter on every page.
- **Feature inference**: scans member file paths for `modules/<feature>/` segment, majority vote,
  fallback to `shared`; primary class slug derived from PascalCase class name (kebab, collision
  suffix `-2`/`-3`).
- **LLM enrichment** (`--llm`): OpenAI-compatible or Anthropic calls via `--llm-provider`;
  rayon parallel with `--llm-concurrency` (default 8); structured `{"po","ba","dev"}` JSON summaries
  injected into community pages; `--llm-dry-run` for local testing; graceful degradation per
  community.
- **`docs-viewer/`**: Docusaurus 3 site in the yummy-cih repo; `CIH_WIKI_PATH=<path> npm start`
  serves any repo's wiki output on port 3001; sidebar auto-generated from the feature folder
  structure; repo name read from `manifest.json`.
- **35 new tests** (23 in `cih-wiki`, 4 in `wiki_cmd`, existing suites unchanged). 160 total.

## Phase 10b — Table-Level DB Access Intelligence ✅ (2026-06-16)

**Primary personas: Dev, BA, PO** — surfaces which methods read/write which Oracle tables.

Banking code uses `DBUtil.prepareStatement / executeQuery / executeUpdate` with static SQL
constants instead of Spring Data repositories. This phase adds first-class DB graph concepts
so the wiki and MCP tools can answer "which methods touch `CUSTOM_OVERDRAFT`?"

- **`NodeKind::DbQuery`** / **`NodeKind::DbTable`**: first-class graph nodes for SQL constants
  and database tables. Stable IDs: `DbQuery:<fqcn>#<const_name>` and `DbTable:<TABLE_NAME>`.
- **`EdgeKind::ExecutesQuery`** / **`ReadsTable`** / **`WritesTable`**: three new edge types
  forming the path `Method → DbQuery → DbTable`.
- **`SqlConstant` + `SqlExecutionSite` IR** (added to `ParsedFile`): parser extracts
  `private static final String SCREAMING_SNAKE_CASE = "..."` constants (with string-literal
  concatenation folding, `dynamic=true` on non-literal parts) and detects `DBUtil.*` /
  `JdbcTemplate.*` execution sites pointing to those constants.
- **`cih-parse/src/sql.rs`**: lightweight Oracle-aware SQL table scanner — strips block/line
  comments, Oracle hints (`/*+ ... */`), schema prefixes; detects `SELECT/FROM/JOIN`,
  `INSERT INTO`, `UPDATE`, `DELETE FROM`, `MERGE INTO`. Conservative: no false positives, some
  missed tables in complex dynamic SQL.
- **`cih-resolve::emit_db_access(&[ParsedFile])`**: links execution sites to constants via
  same-file lookup; runs the SQL scanner; emits `DbQuery` + `DbTable` nodes and DB edges.
  Cross-file constants → `dynamic=true`, no table edges (v1 scope).
- **`DbQuery.props`**: `operation`, `constantName`, `sqlPreview` (first 120 chars),
  `dynamic`, `tables`, `dialect: "oracle-like"`.
- **Engine wiring**: `analyze_from_scope_with_options` calls `emit_db_access` after the resolve
  pass and merges the DB nodes/edges into `nodes.jsonl` / `edges.jsonl`.
- **27 new tests** across `cih-core`, `cih-parse` (SQL scanner + parser integration),
  `cih-resolve` (emit unit tests), `cih-engine` (artifact integration test). **187 total**.

## Phase 10c — Adapter-Based LLM Wiki Enrichment ✅ (2026-06-17)

**Plan:** `docs/plans/phase-10c-llm-adapter-plan.md` (plan file uses the label "10b"; 10c is the correct sequence number to avoid collision with DB access).

Upgrades the existing `--llm` path from an implicit Anthropic-or-OpenAI choice to a pluggable adapter layer with a richer evidence pack and BRD file support.

- **New `cih-engine/src/llm/` module**: `LlmAdapter` trait; `openai-compatible`, `anthropic`, and `http-json` adapters extracted from `wiki_cmd.rs`.
- **`http-json` adapter**: JSON config with `{{prompt}}`/`{{model}}`/`{{api_key}}`/`{{env:VAR}}` substitution and dotted response path — covers Ollama, vLLM, LM Studio, and any custom REST API.
- **Richer evidence pack**: all routes, stereotypes, callers/callees, DB tables, events/topics, bounded source snippets (3 × 10 lines), BRD file chunks (≥2-term match, cap 2 per community, 3 000-char total).
- **New flags**: `--llm-provider <openai-compatible|anthropic|http-json>`, `--llm-provider-config <path>`, `--llm-api-key-env <VAR>`, `--evidence <path>` (repeatable), `--llm-max-tokens` (default 600), `--wiki-language <en|vi>`.
- **Explicit provider selection**: removes `base_url.contains("anthropic.com")` implicit detection.
- **Manifest extension**: `llm_provider`, `llm_language`, `llm_evidence_file_count`, `llm_enriched_community_count`, `llm_failed_community_count`.
- **Migration**: existing `--llm` users with an Anthropic URL must add `--llm-provider anthropic`.
- **Done when:** `wiki --llm --llm-provider http-json --llm-provider-config ollama.json` enriches pages from a local Ollama instance; BRD evidence improves PO page quality; all tests green.

## Phase 15 — Flow Intelligence: trace_flow · feature_map ✅ (2026-06-15)

**Primary personas: PO, BA**

- **`trace_flow(entry_point, max_depth?)`** MCP tool: given an HTTP route or method ID, returns
  the full downstream execution chain as a structured list of `FlowNode` records (id, kind, name,
  file, depth). Traverses `CALLS`, `HANDLES_ROUTE`, `EXTERNAL_CALL`, `PUBLISHES_EVENT`,
  `LISTENS_TO` edges via a single Cypher variable-length path query; depth clamped to 10; results
  ordered by minimum depth then name; cap 100 nodes. Supports full NodeId or short-name
  disambiguation identical to `context` / `impact`.
- **`cr_impact`** — **out of scope for cih-server.** Belongs in yummy-agent (LLM synthesis of
  `query` + `impact` calls). `cih-server` exposes `query` and `impact` as primitives only.
- **`feature_map(query, limit?)`** MCP tool: maps a business keyword string (e.g. `"checkout
  payment"`) to code clusters — BM25 search results grouped by community name via `MEMBER_OF`
  edges. Returns `Vec<{community, symbol_count, symbols: [{id, kind, name, file, score}]}>`;
  unmatched nodes go into an `"unclustered"` group.
- **Implementation:** `FlowNode` struct + `flow_downstream()` + `symbol_communities()` added to
  `GraphStore` trait (`cih-graph-store`); FalkorDB Cypher impls in `cih-falkor`; two new
  `#[tool]` methods in `cih-server`. No changes to engine or parse layers.
- **Verified:** 113 tests green *(at the time)*. Two tools listed in `get_info()` instructions.
- **Done when:** a BA can type "trace the checkout flow" → `trace_flow` returns the controller →
  service → repo → external call chain; a PO asks "what implements order payment?" →
  `feature_map` returns grouped symbol clusters.

## Phase 15.5 — Unresolved Reference Reports + Factory-Aware Resolution ✅ (2026-06-15)

**Pure diagnostics + resolver accuracy.** No new MCP tools; improves graph fidelity and
provides a machine-readable report alongside every analysis run.

- **`UnresolvedRef` struct** added to `cih-resolve`: per-site record with `file`, `kind`,
  `name`, `receiver`, `arity`, `in_fqcn`, `range`, `reason`, `resolved_receiver_type`,
  `external_fqcn`. Reason taxonomy: `receiver_type_unknown`, `receiver_external`,
  `member_not_found`, `ctor_type_unknown`, `type_ref_unknown`, `heritage_type_unknown`,
  `free_call_unresolved`, `field_not_found`.
- **`ResolveOutput.unresolved_refs`** field replaces bare `skipped += 1` with structured
  per-site data. `EdgeEmitter::push_unresolved()` atomically increments `skipped`, updates
  `unresolved_external_fqcns` (backward-compat), and appends an `UnresolvedRef`.
- **`write_unresolved_reports()`** (new `cih-resolve::reports` module) writes
  `unresolved-refs.jsonl` + `unresolved-refs.md` (by-reason table, top-file table, missing
  external FQCNs list) alongside `nodes.jsonl`/`edges.jsonl` every analyze run.
- **Factory-aware `CallResult` resolution** (`ResolveIndex::callresult_via_field_types`):
  when `var x = create()` can't be resolved on the enclosing class, scans declared fields of
  that class; if exactly one field's type has the method, follows its return type. Handles the
  `var order = this.factory.create(); order.process()` pattern without parser changes.
- **Engine wiring:** `cih-engine/analyze.rs` calls `write_unresolved_reports` after artifact
  write; `EmitOutcome.unresolved_report_path` surfaces the path in `print_human` output.
- **Verified:** 118 tests green (5 new in cih-resolve + 1 extended engine integration test).

## Phase 16 — Test Intelligence: coverage · regression scope · untested paths ✅ (2026-06-16)

**Primary persona: Tester**

- **`EdgeKind::Tests`** added to `cih-core`; FalkorDB adapter maps `"TESTS"` ↔ `EdgeKind::Tests`.
- **Test class detection in `cih-parse`:** identifies test classes via `@SpringBootTest`,
  `@ExtendWith`, `@RunWith`, `@WebMvcTest`, `@DataJpaTest`, `@DataMongoTest`, `@JsonTest`, and
  naming conventions (`*Test`, `*Tests`, `*IT`, `*Spec`). Sets `stereotype="test"` on matching class
  nodes. Emits `TESTS` edges from `@Test`/`@ParameterizedTest`/`@RepeatedTest` methods to their
  owner class (confidence 0.8, reason `"test-method"`) and from test classes to types injected via
  `@MockBean`/`@SpyBean`/`@Autowired`/`@InjectMocks`/`@Mock` fields (confidence 0.7, reason
  `"mock-bean"`). `TypeContext.is_test` gating ensures TESTS edges are only emitted from test
  classes.
- **`test_coverage(symbol_id)`** MCP tool (`cih-server`): queries TESTS edges to `id` or its owner
  class; returns up to 50 test nodes.
- **`regression_scope(changed_files)`** MCP tool: given changed repo paths, returns distinct test
  class/method nodes covering any symbol in those files (direct TESTS + one-hop via CALLS, up to
  200 each, merged in Rust).
- **`untested_paths(module_prefix, limit?)`** MCP tool: returns production Method/Class/Interface
  nodes under `module_prefix` that have no inbound TESTS edge (excludes `stereotype="test"` nodes).
- **GraphStore trait:** 3 new methods — `test_coverage`, `tests_for_files`, `untested_symbols`;
  Falkor Cypher impls in `cih-falkor`.
- **Verified:** 125 tests green (4 new parse tests, 2 server arg tests, 1 engine integration test).

## Phase 17 — Visualization output for yummy frontend ✅ (2026-06-16)

**Primary consumer: yummy frontend**

MCP tools that return graph data support a `format` parameter so yummy can render diagrams without
a separate graph-rendering backend:

- `trace_flow(..., format="mermaid")` → Mermaid `flowchart TD` of the execution chain
- `impact(..., format="diagram")` → D3-JSON force-directed blast-radius graph
- `communities(format="diagram")` → D3-JSON service map with inter-community edge weights
- `route_map(format="openapi")` → OpenAPI 3.0.3 JSON of the indexed route surface

**Data additions (additive, no existing callers break):**
- `FlowNode.parent_id: Option<NodeId>` — predecessor in shortest path (enables Mermaid edges)
- `ImpactNode.name/kind/parent_id` — enriches impact JSON and enables D3 graph links
- `CommunityEdge { src, dst, weight }` struct + `GraphStore::community_graph()` trait method +
  Falkor Cypher impl (counts CALLS edges crossing community boundaries)

**New `cih-server/src/viz.rs`:** 4 pure render functions:
- `render_mermaid_flow()` — sanitized node IDs, parent-tracking edges, truncated labels
- `render_d3_impact()` — root node + affected nodes with parent→child links
- `render_community_diagram()` — communities as nodes, inter-community calls as weighted links
- `render_openapi()` — groups routes by path, derives operationIds, adds `x-handler-*` extensions

**Falkor Cypher update:** `impact()` and `flow_downstream()` use a two-step WITH/ORDER BY/collect
pattern to extract the shortest-path parent for each node.

**Verified:** 133 tests green (1 new Falkor test, 5 viz unit tests, 2 new server arg tests).

> **Dev shortcut:** FalkorDB's Docker image ships a browser UI on port 3000. Expose it with
> `"3000:3000"` in docker-compose for direct Cypher graph exploration during development.
> The yummy frontend (this phase) is the product visualization path.

---

## Near-term additions (from GitNexus discovery)

## Phase 18 — Repo registry + MCP resources ✅ (2026-06-14)

Source: `docs/gitnexus-discovery.md` §1 + §2

- **Completed 2026-06-14:**
  - **`~/.cih/registry.json`** — `Registry` / `RegistryEntry` / `RegistryStats` types in
    `cih-core/src/registry.rs`; load/save/upsert/find/is_stale methods; RFC-3339 timestamps
    and git HEAD capture via `std::process::Command` (no new deps).
  - **Auto-register on analyze** — `cih-engine analyze` persists an entry after every
    successful run (`cih-engine/src/registry.rs::persist_analyze`); nodes, edges, and file
    counts come directly from `EmitOutcome`.
  - **Auto-update on discover** — `cih-engine discover` updates `route_count`,
    `community_count`, `process_count`, and `community_artifacts_dir` in the registry entry
    (`persist_discover`). `route_count` is counted from the source nodes already in memory —
    zero extra I/O. `DiscoverOutcome` gained a `route_count` field.
  - **CLI:** `cih-engine list` (tabular or `--json`) + `cih-engine status <name>` (human or
    `--json` with staleness flag).
  - **MCP tools:** `list_repos()` returns all registry entries; `status({ name })` returns one
    entry + staleness boolean.
  - **MCP resources** (`cih-server/src/resources.rs`):
    ```
    cih://repo/{name}/context      → RegistryEntry JSON
    cih://repo/{name}/communities  → Community nodes from community artifacts
    cih://repo/{name}/processes    → Process nodes from community artifacts
    cih://repo/{name}/schema       → { node_kinds, edge_kinds }
    ```
    Resource templates registered for all four URI patterns. Server capabilities updated to
    `enable_tools().enable_resources()`.
  - Workspace: **102 tests** green *(at the time)*, clippy clean.

## Phase 19 — Ambiguous symbol resolution + `detect_changes` ✅ (2026-06-14)

Source: `docs/gitnexus-discovery.md` §3 + §4

**Ambiguous symbol handling:** `context` and `impact` now call `resolve_symbol()` before
forwarding to the store. A short name (no `:` prefix) queries `candidates_by_name()`:
- 0 results → `invalid_params` error
- 1 result → proceeds with the found NodeId
- >1 results → returns `{"status":"ambiguous","candidates":[{id,kind,name,file},...]}` (success)

Full NodeIds (e.g. `Class:com.acme.OrderService`) skip disambiguation entirely.

**`detect_changes` MCP tool:**
```
detect_changes({ scope: "working" | "staged" | "base_ref", base_ref?: string, repo?: string })
```
Returns `{ changed_files, changed_symbols, affected_symbols, affected_processes, risk }`.
Implementation: `git diff --name-only [--cached] HEAD` → `nodes_in_files()` → BFS `impact()`
(up to 20 symbols, upstream, depth 4) → `processes_for_symbols()` → `risk_from_fanout()`.

**New `GraphStore` methods** (`cih-graph-store/src/lib.rs`):
- `candidates_by_name(name, limit)` — exact `n.name` match, returns up to N nodes
- `nodes_in_files(files)` — nodes for changed files (Method/Constructor/Function/Class/Interface/Enum)
- `processes_for_symbols(ids)` — STEP_IN_PROCESS reachable Process nodes

**New helpers** in `cih-server/src/main.rs`:
- `CihServer::graph_key: String` — used to find the default repo in the registry
- `find_repo_path(repo, graph_key)` — registry lookup with graph_key fallback
- `git_changed_files(repo_path, scope, base_ref)` — runs git diff, returns relative paths

- Workspace: **11 server + falkor tests** green, clippy clean.

## Phase 20 — Agent workflow docs ✅ (2026-06-14)

Source: `docs/gitnexus-discovery.md` §5

Created `docs/agent-workflows/` with five skill files:

```
docs/agent-workflows/
  exploring.md          <- orient to an unfamiliar codebase (any persona)
  impact-analysis.md    <- blast-radius workflow (Developer, Tech Lead)
  debugging.md          <- call-chain tracing (Developer)
  product-owner.md      <- route_map + process/community view (PO, BA)
  tester.md             <- regression scope + E2E coverage mapping (Tester, QA)
```

Each file: persona, when-to-use, step-by-step tool calls with example inputs/outputs,
output shape for the agent to return, tips. Every doc references ≥ 3 CIH tools.

Feeds Phase 10 — these docs become the grounding for yummy persona system prompts.

---

## Mid-term additions (from GitNexus discovery)

## Phase 21 — Cross-service contract extraction ✅ (2026-06-15)

Source: `docs/gitnexus-discovery.md` §6

- **HTTP clients:** `@FeignClient`, `RestTemplate`, `WebClient` call sites detected in
  `cih-parse/src/java.rs:682-845` → `ExternalEndpoint` nodes + `EXTERNAL_CALL` edges.
- **Events:** `@KafkaListener`, `KafkaTemplate.send()`, `ApplicationEventPublisher.publishEvent()`,
  `@EventListener` → `KafkaTopic` nodes + `PUBLISHES_EVENT` / `LISTENS_TO` edges.
- **New core types:** `NodeKind::{KafkaTopic,ExternalEndpoint}`, `EdgeKind::{PublishesEvent,ListensTo,ExternalCall}`,
  `ContractSite` / `ContractKind` IR types in `cih-core`; `GroupRegistry` / `GroupEntry` /
  `ContractMatch` in `cih-core/src/group.rs`.
- **`resolve_contract_edges()`** integrated into `cih-resolve::resolve_edges()` — deduplicates
  topic/endpoint nodes and emits typed edges.
- **CLI:** `cih-engine group create/add/remove/list/sync` in `cih-engine/src/group_cmd.rs`;
  `sync_group()` reads JSONL artifacts, matches HTTP routes to ExternalEndpoint consumers and
  Kafka publishers to listeners across repos, writes `~/.cih/groups/<name>/contracts.jsonl`.
- **MCP tool:** `group_contracts({ group, kind? })` in `cih-server` reads the contracts artifact
  and returns matched provider/consumer pairs.
- **Schema resource** updated with new node/edge kinds.
- **Verified 2026-06-15:** `212ecom-be` analyze emits 6 KafkaTopic nodes
  (`OrderCreatedEvent`, `OrderStatusChangedEvent`, `OrderCancelledEvent`, `LowStockEvent`,
  `CriticalStockEvent`, `ActivityLoggedEvent`); `group sync` and MCP `group_contracts` tool
  return correct JSON. All 111 workspace tests green.

## Phase 22 — API impact + shape check ✅ (2026-06-15)

Source: `docs/gitnexus-discovery.md` §7

Builds on Phase 21 HTTP contracts:

- **`api_impact({ group, method, path })`** — return all consumers of an HTTP route across the group.
- **`shape_check({ group, provider, consumer })`** — compare response DTO fields of provider against
  property accesses of consumer; flag mismatches.

- **Done when:** `api_impact({method:"GET",path:"/orders/{id}"})` returns the consuming services. ✅
- **Completed 2026-06-15:**
  - `normalize_contract_path()` moved to `cih-core/src/group.rs` (pub); engine now delegates to it.
  - Method `Node.props["returnType"]` persisted in `nodes.jsonl` via `cih-parse/src/java.rs` so
    `shape_check` can identify response DTO classes without live type resolution.
  - `api_impact({ group, method, path })` MCP tool: reads `contracts.jsonl`, normalizes path to
    `METHOD /normalized/path` key, returns all consumer repos + their ExternalEndpoint node ids.
  - `shape_check({ group, provider, consumer })` MCP tool: loads both repos' artifacts; for each
    HttpRoute contract, diffs provider response-DTO fields (via returnType → class → HasField edges)
    against consumer Accesses edges; reports `matched` / `provider_only` / `consumer_only` fields.
  - Workspace: **111 tests** green (no new tests added; tools verified via build).
  - **Known gap:** `api_impact` and `shape_check` have no dedicated unit tests — covered by build
    verification only. Add tests before relying on these tools in production.

## Phase 23 — Generated wiki ✅ (covered by Phase 10a + docs-viewer)

Source: `docs/gitnexus-discovery.md` §10

`cih-engine wiki` (Phase 10a) delivers the full wiki generation pipeline. `docs-viewer/` (added
2026-06-16) is the Docusaurus 3 viewer: `CIH_WIKI_PATH=<repo>/.cih/wiki/pages npm start`.
Feature-first hierarchy, per-feature PO/BA/Dev pages, and LLM enrichment are all in place.
No further work needed for this milestone.

## Phase 24 — Graph-assisted rename dry-run

Source: `docs/gitnexus-discovery.md` §8

```
rename_symbol({ id, new_name, dry_run: true })
```

Returns: graph-confirmed edits, text-search candidates, ambiguous/unsafe edits, tests to re-run.
Developer persona safe-refactoring workflow.

---

## Later additions (from GitNexus discovery)

## Phase 25 — CFG/PDG + taint analysis

Source: `docs/gitnexus-discovery.md` §9

```
cih-engine analyze <repo> --pdg
explain({ target })
```

Emits `BasicBlock` nodes, `CFG` / `REACHING_DEF` / `TAINTED` / `SANITIZES` edges.
Target: security review and injection-path analysis for banking/fintech codebases.
Priority: low now, high if security analysis becomes a product goal.

## Phase 26 — Multi-repo group + cross-repo impact

Full cross-repo group sync (Phase 21 covers per-pair contracts; this is full group-level
orchestration). Cross-repo `impact` traverses service boundaries across the entire group.
Includes additional JVM language support (Kotlin) via new `LanguageProvider` impls.

---

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

## Phase 13 — Spring DI-aware resolution ✅ (2026-06-14)

- **Build:** bean-wiring pass — resolve `@Autowired`/constructor injection so an interface-typed
  call (`UserService service; service.save()`) routes to the concrete `@Service` impl; augment
  receiver type bindings before the receiver-bound pass.
- **Completed 2026-06-14:**
  - **`stereotype` field on `SymbolDef`** (`cih-core/src/ir.rs`) — Spring stereotype propagated
    through the parse IR so the resolver can see it; `#[serde(default)]` keeps cached artifacts
    backward-compatible.
  - **Parse-time population** (`cih-parse/src/java.rs`) — `class_stereotype()` result stored into
    every type-kind `SymbolDef` at parse time; reuses existing annotation helper, zero new parsing.
  - **`type_stereotypes` index in `ResolveIndex`** (`cih-resolve/src/lib.rs`) — built in Pass 1
    alongside `types_by_fqcn`; `is_spring_bean()` helper matches `service|repository|component|
    controller|configuration`.
  - **`di_impl()` + DI redirect in `resolve_receiver_bound_call()`** — when receiver resolves to
    an interface, look up the single `@Service`/`@Repository`/… implementor; if unambiguous,
    redirect the `CALLS` edge to the concrete impl method (confidence 0.9, reason `"di-resolved"`).
    Falls back to interface method when 0 or ≥2 bean impls (no silent wrong-impl guess).
  - 5 new tests (`di_resolves_interface_call_to_service_impl`, `di_falls_back_when_no_service_impl`,
    `di_falls_back_when_multiple_service_impls`, `di_not_applied_to_concrete_class_receiver`,
    `di_resolves_repository_interface`). Workspace: **98 tests** green *(at the time)*, clippy clean.
- **Done when:** interface calls resolve to the impl in `impact`/`call_chain`. ✅

## Phase 14 — More languages (generic-pipeline payoff)

- **Build:** add `LanguageProvider` impls (Kotlin next, then others) reusing the generic pipeline;
  per-language scope query + MRO strategy only.
- **Done when:** a second language indexes through the unchanged engine.

---

## Sequencing & parallelism

Phases 1–22 are ✅ complete. What remains:

- **Phase 10c** (LLM adapter) — ✅ done.
- **Phase 10** (product — yummy frontend + Agent SDK) — all MCP tools, wiki, registry, and workflow
  docs are ready. Only the Next.js BFF and persona chat UI remain.
- **Phase 11** (storage spike) — benchmark FalkorDB / Postgres-CTE / Neptune; needed before Phase 12.
- **Phase 12** (AWS go-live) — requires Phase 11 backend decision; independent of product phases.
- **Phase 14** (more languages) — self-contained; add after the core is proven in production.
- **Phase 24** (rename dry-run), **Phase 25** (CFG/taint), **Phase 26** (multi-repo group) — deferred;
  no blocking dependency on near-term phases.
- **Phase 8 full-decompile** — rare exception path; proceed only when a specific dep requires it.

## Definition of done (overall v1)

Index the real Java/Spring repo (incl. decompiled deps) → accurate call graph in FalkorDB (dev) /
Neptune (prod) + vectors in pgvector → MCP tools + a Claude-Agent-SDK chat product answer
impact/architecture questions with grounded citations, on banking-grade AWS.
