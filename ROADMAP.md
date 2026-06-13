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
  `upsert_incremental` / `swap_version` on `FalkorStore`; **switch queries to FalkorDB query
  parameters** (drop the stub inline-escaping).
- **Done when:** load a synthetic ~10k-node graph; queries return; re-load is idempotent.
- **VERIFIED 2026-06-13:** canonical artifacts are **JSONL** (`nodes.jsonl`/`edges.jsonl`) in
  `cih-core` (`GraphArtifacts::write`/`read_nodes`/`read_edges`) — Parquet deferred to the Neptune
  S3 path (Phase 11). `FalkorStore::{bulk_load,upsert_incremental,swap_version}` + `FalkorBulkLoader`
  implemented (UNWIND-batch MERGE, idempotent; edges grouped per type; `_CihMeta` version node;
  `upsert` = delete-by-file then re-load). Read queries now use FalkorDB `CYPHER` parameters.
  Smoke test: `cargo run -p cih-falkor --example load_sample` → impact returns the full call chain.
  Remaining polish: Parquet + S3 (Phase 11), blue-green staging-key swap, compact-protocol list
  parsing for `call_chain`.

## Phase 3 — Engine MVP: scan + parse + structure  🎯 first vertical slice

- **Crates:** `cih-parse`, `cih-lang` (JavaProvider), `cih-engine` (orchestrator); extend `cih-core` IR.
- **Build:** file scan with ignore rules; tree-sitter-java parse via the scope query
  (port `languages/java/query.ts`); extract `Class`/`Interface`/`Method`/`Field` defs + `File`/
  `Folder` nodes + `CONTAINS`/`HAS_METHOD` edges → emit `GraphArtifacts`; `rayon` parallel parse;
  wire engine → `BulkLoader` → FalkorDB.
- **Done when:** `cih-engine analyze <java-repo>` loads real symbols; MCP `context` shows them.
  (Calls still absent/crude — that's Phase 4.) 🎯 **Milestone: index → query a real repo.**

## Phase 4 — Scope resolution + MRO  🎯 accurate call graph

- **Crate:** `cih-resolve`.
- **Build:** `finalize_scope_model` (def + qualified-name index); the 5 emit passes —
  `emit_receiver_bound_calls` (7-case dispatcher), `emit_free_call_fallback`,
  `emit_references_via_lookup`, `emit_import_edges` (ports: `scope-resolution/pipeline/run.ts`,
  `passes/receiver-bound-calls.ts`, `graph-bridge/ids.ts`); C3 MRO (port `mro-processor.ts`).
  Emit `CALLS`/`ACCESSES`/`USES`/`EXTENDS`/`IMPLEMENTS`/`METHOD_OVERRIDES` with `confidence`.
- **Done when:** index `spring-petclinic`; `service.save()` resolves to the right method;
  `impact()` returns real callers; node/edge counts sane vs GitNexus on the same repo.
  🎯 **Milestone: trustworthy impact analysis.**

## Phase 5 — Communities + processes

- **Build:** Leiden over CSR (port `community-processor.ts` params: resolution 2.0/1.0, capped
  iters, >10k filtering) → `Community` nodes + `MEMBER_OF`; process BFS (port
  `process-processor.ts`: depth 10 / branch 4 / max 75) → `Process` nodes + `STEP_IN_PROCESS`.
- **Done when:** clusters + execution flows appear; MCP resources expose them.

## Phase 6 — Search: BM25 + embeddings + hybrid

- **Crates:** `cih-search`, `cih-embed` (or a TS/Python embedding sidecar).
- **Build:** in-Rust BM25 over name+content (port `bm25-index.ts`); chunker + embeddings
  (`fastembed`/`ort` or sidecar, MiniLM-384 / bge-m3-1024) → **pgvector HNSW**; hybrid RRF
  (`k=60`, port `hybrid-search.ts`); wire the `query` MCP tool → hybrid search → seed nodes +
  `subgraph`.
- **Done when:** `query("user registration")` returns ranked, process-grouped results.

## Phase 7 — Spring pre-phase

- **Crate:** `cih-spring`.
- **Build:** stereotype tags (`@Service`/`@Repository`/`@Controller`/`@RestController`/
  `@Configuration`/`@Component`, `@Bean`); routes (`@RequestMapping`/`@GetMapping`/… → `Route` +
  `HANDLES_ROUTE`); JPA (`@Entity`, `JpaRepository`/`CrudRepository`) tags.
- **Done when:** routes + beans are queryable; a `route_map` view works on a Spring app.

## Phase 8 — Decompilation (Fernflower)

- **Build:** engine spawns Fernflower on dependency JARs **lacking source** → `.java` into
  `EFS:.workspace-dependencies/`; index as first-class source; tag decompiled nodes;
  size-skip guard for generated/oversized files.
- **Done when:** calls into a decompiled dependency resolve instead of being silently dropped.

## Phase 9 — Incremental re-index + cache + versioning

- **Build:** blake3 file-hash diff vs prior `meta.json`; parse cache (bincode, content-addressed);
  importer-BFS expansion (depth 4); atomic version swap in the store (port `run-analyze.ts`).
- **Done when:** re-index after editing one file is fast and correct (only the delta re-resolves).

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
