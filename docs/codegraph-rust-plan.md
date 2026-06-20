# Plan: A Rust code-intelligence engine (GitNexus-inspired), Java/Spring-first

## Context

GitNexus (TypeScript/Node) indexes a codebase into a knowledge graph and serves it to AI
agents. Its CPU/memory-heavy stages run as JS on the main thread over string-keyed `Map`s,
which is V8's worst case for both throughput and memory (~200–400 B/node; ~3–4 GB heap on a
100k-function repo). The genuinely native stages (tree-sitter parse, ONNX embeddings, DB
load/FTS) are already fast and language-agnostic.

Goal: rebuild the engine as a **single Rust binary** that keeps GitNexus's *design* for the
native stages but reimplements the JS-bound stages — **scope resolution, MRO, graph
build/merge, BM25 + Leiden** — in Rust for ~2–4× lower memory, lower tail latency, and true
multi-threaded resolution (a place GitNexus is structurally single-threaded). Target a
**Java Spring backend** first, but keep the pipeline generic so more languages slot in later.

Decisions locked in:
- **Full Rust core** — parse (tree-sitter-rs), embeddings (ONNX via Rust), DB access all in Rust.
- **Kuzu** as the embedded graph DB (Cypher + vector index) — the LadybugDB-equivalent.
- **Generic resolution first**; Spring DI-aware (interface→@Service impl) resolution is phase 2.
- **CLI + Rust library** as the v1 surface; MCP/HTTP servers come later.

Reference implementation to port from (paths under `GitNexus/gitnexus/src`):
- Graph store: `core/graph/graph.ts` (the Map/Set indexes to replace with columnar + CSR).
- Scope resolution order: `core/ingestion/scope-resolution/pipeline/run.ts`.
- ID lookup cascade: `core/ingestion/scope-resolution/graph-bridge/ids.ts` + `node-lookup.ts`.
- Receiver-bound dispatch (7 cases): `core/ingestion/scope-resolution/passes/receiver-bound-calls.ts`.
- MRO: `core/ingestion/mro-processor.ts`.
- Leiden: `core/ingestion/community-processor.ts` (resolution 2.0/1.0, maxIter 3/0, >10k filtering).
- Processes BFS: `core/ingestion/process-processor.ts` (depth 10, branch 4, max 75).
- BM25/RRF: `core/search/bm25-index.ts`, `core/search/hybrid-search.ts` (`RRF_K = 60`).
- IR shapes: `gitnexus-shared/src/scope-resolution/{parsed-file,reference-site,symbol-definition,types}.ts`.
- Java query: `core/ingestion/languages/java/query.ts`.

## Architecture — Cargo workspace

```
yummy-cih/
  crates/
    cih-core/     ids, interner, NodeKind/EdgeKind, Range, ParsedFile IR (serde+bincode)
    cih-parse/    tree-sitter parse + scope-query capture → ParsedFile (rayon-parallel)
    cih-lang/     LanguageProvider trait + JavaProvider (query, call-form, MRO strategy)
    cih-graph/    columnar GraphStore + CSR adjacency + node-key lookup + merge
    cih-resolve/  finalize → emit passes (scope resolution) + MRO
    cih-spring/   Spring pre-phase: bean/route/JPA extraction (your customization point)
    cih-search/   BM25 index + RRF hybrid + vector search
    cih-embed/    ONNX embeddings (fastembed/ort) + chunker — opt-in
    cih-store/    Kuzu schema, bulk COPY load, incremental writeback, Cypher query
    cih-index/    pipeline orchestration (scan→…→persist) + parse cache
    cih-cli/      clap CLI: analyze, query, context, impact, status
```

Key dependencies: `tree-sitter` + `tree-sitter-java`; `rayon` (data-parallel — replaces Node
worker_threads + the disk ParsedFile store entirely, since threads share memory);
`lasso::ThreadedRodeo` (concurrent string interning); `kuzu` (official Rust bindings);
`fastembed` (ONNX + bundled Snowflake arctic-embed-xs, 384-dim — matches GitNexus);
`blake3` (content-hash cache keys); `serde`+`bincode` (parse cache); `clap`; `anyhow`/`thiserror`.

## Core types (cih-core) — the memory win starts here

- Interned strings: `type Sym = lasso::Spur;` Newtype IDs: `NodeId(u32)`, `ScopeId(u32)`, `FileId(u32)`.
- `enum NodeKind` / `enum EdgeKind` mirroring `gitnexus-shared/src/graph/types.ts` (`NodeLabel`/`RelationshipType`).
- `struct Range { start_line, start_col, end_line, end_col: u32 }`.
- IR (mirror `parsed-file.ts`): `ParsedFile { file: FileId, module_scope: ScopeId, scopes: Vec<Scope>, parsed_imports: Vec<ParsedImport>, local_defs: Vec<SymbolDef>, reference_sites: Vec<ReferenceSite> }`.
  - `ReferenceSite` mirrors `reference-site.ts`: `{ name: Sym, at: Range, in_scope: ScopeId, kind: RefKind, call_form: Option<CallForm>, explicit_receiver: Option<Sym>, arity: Option<u16> }`.
  - `SymbolDef` mirrors `symbol-definition.ts`: `{ node_id, file, kind, qualified_name, param_count, param_types, return_type, declared_type, owner_id, ... }`.

## Graph store (cih-graph) — replaces graph.ts

Struct-of-arrays indexed by `NodeId(u32)` instead of `Map<string, {id,label,properties}>`:
```rust
struct GraphStore {
  node_kind: Vec<NodeKind>, node_name: Vec<Sym>, node_file: Vec<FileId>, node_range: Vec<Range>,
  node_props: Vec<Option<Box<NodeProps>>>,           // rare/optional fields boxed off the hot array
  edges: Vec<Edge>,                                   // {src, dst: NodeId, kind: EdgeKind, conf: f32}
  // indexes (mirror graph.ts):
  node_by_qkey: HashMap<(FileId, NodeKind, Sym), NodeId>,  // qualifiedKey
  node_by_skey: HashMap<(FileId, Sym), NodeId>,            // simpleKey fallback
  edges_by_kind: [Vec<u32>; N_EDGE_KINDS],                 // relationshipsByType
  nodes_by_file: HashMap<FileId, Vec<NodeId>>,             // nodeIdsByFile
  // built once after ingestion for traversal:
  csr_out: Csr, csr_in: Csr,                               // replaces edgeIdsByNode Set-of-Sets
}
```
- Node lookup bridge (port `resolveDefGraphId` cascade): tiered probe — param-shape key →
  param-types key → arity key → qualified key → simple key. Same disambiguation, plain functions.
- CSR adjacency (compressed sparse row) built after the graph is final → O(1) neighbor
  iteration for Leiden, processes BFS, and impact traversal (GitNexus uses HashMap-of-Sets).
- Expected ~40–60 B/node vs JS ~200–400 B → the 2–4× memory reduction.

## Parse (cih-parse) + Lang (cih-lang)

- `trait LanguageProvider { fn ts_language(&self); fn scope_query(&self) -> &Query;
  fn classify_call_form(&self, n: Node) -> CallForm; fn is_super_receiver(&self, name) -> bool;
  fn mro_strategy(&self) -> MroStrategy; /* receiver-binding hooks */ }`
- `JavaProvider`: port the scope query from `languages/java/query.ts` (`@scope.*`,
  `@declaration.*`, `@import.statement`, `@type-binding.*`, `@reference.*`). MRO strategy =
  C3 over single superclass + interfaces.
- Parse loop: `rayon::par_iter` over scanned files; each runs tree-sitter + query captures and
  builds a `ParsedFile`. Interning via `ThreadedRodeo`. **No IPC, no disk ParsedFile store** —
  this deletes a whole class of GitNexus complexity (`captureSideChannel`, worker MessageChannel).

## Resolve (cih-resolve) — port run.ts pass order

1. `finalize_scope_model` — build scope tree, def index, qualified-name index, and the
   method/type registries from all `ParsedFile`s (mirror `finalize-orchestrator`).
2. `emit_receiver_bound_calls` — the load-bearing 7-case dispatcher (`receiver-bound-calls.ts`):
   super → compound → namespace → class-name/static → dotted-typebinding → chain-typebinding →
   simple-typebinding → value-receiver. For `service.save()`: receiver `service` → field
   `declared_type = UserService` → class def → MRO walk + `find_owned_member("save", arity=1)`.
3. `emit_free_call_fallback` — bare calls via lexical chain.
4. `emit_references_via_lookup` — drain remaining refs; skip `handled_sites`.
5. `emit_import_edges`.
6. **MRO pass** (`mro-processor.ts`): build EXTENDS/IMPLEMENTS adjacency over CSR, C3-linearize
   (cached), emit METHOD_OVERRIDES / METHOD_IMPLEMENTS.
- Edges carry `confidence: f32` + optional evidence (port `evidence-weights.ts`). Unresolved
  target → drop + `skipped` counter (same semantics as `references-to-edges.ts:67`).
- **Parallelism opportunity**: build the def/qualified-name index once, then resolve reference
  sites with `rayon` (read-only shared index, per-site edge emission into thread-local buffers,
  merged after). This beats GitNexus's single-threaded resolution structurally, not just by constant factor.

## Spring pre-phase (cih-spring) — your customization point

Runs between parse and resolve. v1 (generic resolution first) = **nodes + framework tags**:
- Stereotypes: `@Component/@Service/@Repository/@Controller/@RestController/@Configuration` →
  tag class nodes as beans; `@Bean` methods → bean producers.
- Routes: `@RequestMapping/@GetMapping/@PostMapping/...` → Route nodes + HANDLES_ROUTE
  (port `route-extractors` concepts).
- JPA: `@Entity`, interfaces extending `JpaRepository/CrudRepository` → entity/repo tags.
- **Phase 2 (deferred): DI-aware resolution** — resolve `@Autowired`/constructor injection so an
  interface-typed field (`UserService`) routes calls to its concrete `@Service` impl. This is the
  main value-add over GitNexus's generic resolver; implement as a wiring pass that rewrites/augments
  receiver type bindings before `emit_receiver_bound_calls`.

## Communities, Processes, Search

- **Leiden** (`community-processor.ts`): undirected weighted graph from CALLS/EXTENDS/IMPLEMENTS
  over CSR; >10k symbols → drop conf<0.5 edges + degree-1 nodes; resolution 2.0, capped iters,
  seeded RNG (deterministic). **Risk: no mature Rust Leiden crate** — port the vendored JS
  (`vendor/leiden/index.cjs`) or implement Leiden over `petgraph`; Louvain is an acceptable v1
  fallback. Emit Community nodes + MEMBER_OF.
- **Processes** (`process-processor.ts`): BFS from detected entry points over the CALLS CSR;
  caps depth 10 / branch 4 / max 75; cycle detection. Emit Process nodes + STEP_IN_PROCESS.
- **Search** (`bm25-index.ts` + `hybrid-search.ts`): in-Rust BM25 over name+content (k1/b,
  idf, avgdl); vector search via Kuzu vector index (or `hnsw_rs`); `merge_with_rrf` with
  `RRF_K=60`. Trivial ports.

## Embeddings (cih-embed) — opt-in, off by default

`fastembed` with `Snowflake/snowflake-arctic-embed-xs` (384-dim, matches GitNexus). AST-aware
chunker (1200 chars, 120 overlap). Batch; persist vectors to Kuzu `CodeEmbedding`. Gated behind
a CLI flag exactly like GitNexus (`analyze` runs without embeddings by default).

## Store (cih-store) — Kuzu

- Schema: node tables per kind (or one `Symbol` table + kind col) + `CodeRelation` rel table
  with `type`/`confidence`/`reason` + `CodeEmbedding` table (mirror `core/lbug/schema.ts`).
- Bulk load: stream the columnar store to Parquet (parallel) → Kuzu `COPY FROM` (mirror
  `lbug-adapter.ts` streaming COPY). Single-writer transaction.
- Incremental: blake3 file-hash diff vs prior `meta.json` → delete changed files' nodes/edges
  (`nodes_by_file`) → re-insert, plus importer BFS expansion depth 4 (port `run-analyze.ts`).
- Query: Cypher via Kuzu for `context`/`impact`/`query` CLI commands.

## Pipeline (cih-index) + CLI (cih-cli)

`scan → structure (File/Folder + CONTAINS) → parse (rayon) → spring-prephase → resolve(scope) →
mro → communities → processes → bm25-index → [embeddings] → persist(Kuzu)`.
Parse cache: blake3 per file → bincode `ParsedFile` on disk; skip re-parse on unchanged (graph
is still fully re-resolved for correctness — same invariant as GitNexus).
CLI (`clap`): `analyze [--embeddings]`, `query <text>`, `context <symbol>`, `impact <symbol>`, `status`.

## Milestones

1. **Skeleton + types**: workspace, `cih-core` IR/IDs/interner, `cih-graph` columnar store + CSR + lookup.
2. **Parse Java**: `cih-parse` + `JavaProvider` query → `ParsedFile`; rayon parallel; structure phase.
3. **Resolve + MRO**: port the 5 emit passes + C3 MRO; get `service.save()` → CALLS working.
4. **Persist + query**: Kuzu schema, COPY load, `context`/`impact` via Cypher; CLI `analyze`/`query`.
5. **Search + communities + processes**: BM25/RRF, Leiden, process BFS.
6. **Spring pre-phase**: stereotypes/routes/JPA tags. (DI-aware resolution = later phase 2.)
7. **Embeddings (opt-in)** + incremental re-index + parse cache.

## Risks

- **Resolution edge-case corpus is the moat**, not Rust. Start Java-only, accept lower recall,
  add cases driven by the real Spring repo. Mirror GitNexus's `test/integration/resolvers/java.test.ts`.
- **Leiden**: no strong Rust crate (port or Louvain fallback).
- **Kuzu Rust bindings**: validate COPY throughput + vector index early (milestone 4) before committing.
- **fastembed model parity** with GitNexus output.

## Verification

- **Correctness**: index `spring-petclinic` (small, well-known Spring repo); spot-check that
  `service.save()`-style interface calls resolve as expected; compare node/edge counts and a
  handful of `impact`/`context` results against GitNexus run on the same repo.
- **Per-resolver unit tests**: one test per receiver-bound case (port from `java.test.ts`).
- **Performance**: index the real large repo (~12.7k files / 100k fns); record per-phase
  timings and peak RSS; compare against `gitnexus analyze --verbose` on the same machine.
  Success target: ≥2× lower peak memory and resolution stage no longer the single-thread bottleneck.
