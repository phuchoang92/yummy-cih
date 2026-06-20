# CIH — Code Intelligence Hub: Architecture & Build Plan

> Supersedes `codegraph-rust-plan.md` as the top-level plan. That file is folded in here
> as the **Rust engine internals** reference (the `cih-*` crate designs still apply, now
> packaged as a napi-rs module rather than a standalone binary).

## Context

CIH is a cloud **GraphRAG product** over Java/Spring codebases (including decompiled
third-party dependencies). It indexes code into a knowledge graph + vector store and serves
it through a chat UI, an auto-generated wiki, and an MCP endpoint for editor agents.

The source-of-truth system shape is the `high-architecture` Mermaid diagram (4 layers:
Yummy App → MCP compute task → AWS EFS storage → AWS RDS Postgres). This plan keeps that
architecture and slots a **Rust compute core** in place of the TS worker-thread pool, because
the CPU/memory-heavy stages (scope resolution, MRO, graph build/merge, BM25, Leiden) are
exactly the worker-pool's job and are where Node is weakest.

Decisions locked in:
- **Hybrid**: CIH stays a TypeScript product (MCP server, Postgres, Fernflower, Next.js,
  Docusaurus, AWS). The **worker-pool compute core becomes a Rust native module (napi-rs)**.
- **Storage**: split stores — **pgvector (RDS) for embeddings only**, and a **dedicated,
  pluggable graph DB for the call graph**. Rollout: **FalkorDB (open-source) NOW for dev**, then
  **Amazon Neptune at go-live** (banking-grade compliance PCI/HIPAA/SOC/FedRAMP; AWS-native).
  Both speak openCypher → same `CypherGraphStore`, swap via `CIH_GRAPH_BACKEND`. **Kuzu
  eliminated** (Apple acquisition Oct 2025, OSS abandoned; GitNexus's "LadybugDB" is a fork).
  Postgres-CTE remains the ~$0 fallback adapter. 3-way spike (FalkorDB/Neptune/Postgres-CTE)
  validates traversal latency.
- **LLM orchestration**: **Claude Agent SDK (TS)** in the Next.js BFF, consuming CIH's own MCP
  tools + Claude (Opus 4.8 reasoning / Sonnet 4.x chat). LangGraph.js only if complex multi-stage
  flows are needed; Rig (Rust) only if unifying orchestration into the engine.
- **Embeddings stay in TypeScript** (transformers.js, per the diagram) — not in the Rust core.
- **Java/Spring first**, generic pipeline; Spring DI-aware resolution is a later phase.
- **MCP server = Rust (`rmcp` + `axum`, Streamable HTTP) hosting the engine in-process** —
  recommended. This **supersedes the napi boundary**: engine + MCP are one Rust binary (lowest
  memory/latency, single language for heavy lifting). Next.js talks to it over Streamable HTTP.
  *Caveat to confirm:* moves the MCP server out of TS and re-raises embeddings (move to Rust
  `fastembed`/`ort`, or keep a small TS/Python embedding sidecar). TS `@modelcontextprotocol/sdk`
  remains the mature fallback if staying in TS matters — workload is I/O-bound, so little perf lost.
- **Graph storage is pluggable** via a `GraphStore` port (ports & adapters) — any graph DB or
  Postgres-CTE plugs in behind one interface (see "Pluggable graph storage").

## System architecture (from `high-architecture`, Rust core marked)

```
1. App/UI (Yummy App)
   Next.js frontend (chat + wiki) ⇄ TS/Node backend API
2. Compute & AST (MCP Task) — ⟦RUST SERVICE⟧ cih-engine (rmcp + axum, Streamable HTTP/JSON-RPC)
   ├─ hosts the engine in-process: parse(tree-sitter-rs) → scope-res → MRO → graph build
   │     → Leiden → processes → BM25  (no napi boundary)
   ├─ spawns JetBrains Fernflower (decompile dep bytecode → .java)
   └─ embeddings: Rust fastembed/ort (unify) OR small TS/Python sidecar (MiniLM-L6-v2 / bge-m3)
   [fallback: TS MCP server + Rust core via napi, if staying in TS]
3. Storage (AWS EFS): decompiled .java (.workspace-dependencies/) + Docusaurus wiki (*.meta.md)
4. Data stores (banking-grade, AWS): pgvector on RDS (embeddings) + Amazon Neptune (call graph, openCypher)
```

## MCP server (language/framework) — Rust `rmcp` + `axum`

MCP servers are **I/O-bound** (graph + vector queries + LLM dominate, not server CPU), so the
choice is driven by architecture, not RPS. Benchmarks for reference: Rust `rmcp`+axum ~4,845 RPS
/ ~11 MB; Go (official SDK) ~3,616 / ~18 MB; Node/TS lower / ~200 MB. Transport: **Streamable
HTTP** (SSE deprecated 2025-03-26, removal mid-2026).

**Decision: Rust `rmcp` (official, stable v1.x) + `axum`, hosting the engine in-process.** This
collapses the napi boundary — the MCP service IS the engine (one binary, lowest memory for
multi-tenant, single language). MCP tools (`query`/`context`/`impact`/`detect_changes`/`rename`)
map 1:1 to `GraphStore` methods. Next.js consumes it over Streamable HTTP.

**Fallback (TS MCP server + napi):** if staying in TS, keep `cih-engine` as a napi-rs module in
the Node process; the boundary carries only **file paths to artifacts** (`nodes.parquet`,
`edges.parquet`, `chunk-specs.json`, `stats.json`) — never marshaled arrays — via a napi
`AsyncTask`. Little real perf lost given the I/O-bound workload, at the cost of an FFI seam.

## Rust compute core (`cih-engine`) — the worker-pool replacement

Cargo workspace; crate designs carry over from `codegraph-rust-plan.md`:
`cih-core` (interned ids, IR), `cih-parse` (tree-sitter-rs + rayon), `cih-lang` (JavaProvider +
scope query ported from `languages/java/query.ts`), `cih-graph` (columnar SoA + CSR — replaces
`core/graph/graph.ts` string-keyed Maps), `cih-resolve` (the 5 emit passes from
`scope-resolution/pipeline/run.ts` + C3 MRO from `mro-processor.ts`), `cih-spring` (pre-phase),
`cih-search` (BM25 from `bm25-index.ts`, RRF `RRF_K=60` from `hybrid-search.ts`, Leiden from
`community-processor.ts`, processes from `process-processor.ts`), plus **`cih-engine`** (the
napi wrapper that orchestrates and writes the COPY artifacts).

The four optimized stages and their wins are unchanged from `codegraph-rust-plan.md`:
columnar graph (~2–4× less memory), rayon-parallel scope resolution (GitNexus is
single-threaded here), C3 MRO, in-Rust BM25, Leiden over CSR.

## Decompilation (Fernflower)

MCP spawns Fernflower (async child process, per diagram) on dependency JARs that **lack
source**, emitting `.java` into `EFS:.workspace-dependencies/`. The Rust core then reads those
as first-class source files. Notes:
- Only decompile artifacts with no source available; index real source directly when present.
- Decompiled output carries synthetic members (`lambda$..`, bridge methods, `access$..`) and
  occasionally won't re-parse — accept lower fidelity on decompiled deps and tag those nodes.

## Storage — split stores (pgvector + Neptune), abstracted

**Vectors → pgvector (RDS Postgres):**
- `embeddings(node_id, chunk_index, start_line, end_line, vector vector(D), content_hash)` with a
  **pgvector HNSW** index; `D` = embedding model dim (384 MiniLM / 1024 bge-m3). `node_id` is the
  join key back to the graph DB.

**Call graph → Amazon Neptune Database (recommended):**
- Nodes: `{id, kind, name, qualified_name, file, start_line, end_line, props...}`.
- Edges: `{src_id, dst_id, kind (CALLS/EXTENDS/IMPLEMENTS/CONTAINS/...), confidence, reason}`.
- Traversal (impact / call-chain, depth 6+) via **openCypher** variable-length paths.
- Bulk load: Rust core writes Neptune-format **CSV → S3 → Neptune bulk loader** (clean fit; no
  per-row marshaling). Drivers are a non-issue — load is file-based; the TS backend queries
  openCypher over HTTPS (SigV4).
- Why Neptune: deepest banking compliance (PCI/HIPAA/SOC/FedRAMP), AWS-native (same VPC as RDS),
  managed/Multi-AZ. Since **Leiden + processes run in the Rust core**, the graph DB only needs
  store + traverse — Neptune's sweet spot (no need for Neo4j GDS / built-in algorithms).

**Abstraction + 3-way spike (before final commit):** put both stores behind a `GraphStore`
interface; benchmark **Neptune vs Neo4j Aura vs Postgres recursive-CTE** on the real repo —
impact-traversal latency at depth 2/4/6/8 + bulk-load time. Criteria: deep-traversal latency,
compliance/ops fit (banking + managed favors Neptune), cost, vendor risk. Alternatives:
**Neo4j Aura** if you want richest Cypher/tooling; **FalkorDB** if raw perf+cost dominate and a
younger vendor is acceptable.

**Dual-store consistency:** hybrid search = pgvector returns `node_id`s → Neptune expands the
subgraph by those IDs. Bulk-load BOTH stores from the **same Rust artifacts** per index run and
version/swap atomically so vectors and graph never drift.

### Cost (graph store) — and a cost-aware rollout

Neptune pricing (US-East, Jun 2026): serverless **$0.1098/NCU-hr** (1 NCU ≈ 2 GB; floor **1 NCU
~$80/mo, does NOT scale to zero**); provisioned `db.r6g.large` ~$226/mo; storage $0.10/GB-mo
(graph is ~1–10 GB → negligible); I/O $0.20/M (Standard) or use **I/O-Optimized** (flat, $0/I/O)
for query-heavy GraphRAG. Estimates: **dev ~$80–160/mo**, **prod 1-repo Multi-AZ ~$450–750/mo**,
multi-tenant **$1k+**. This is **additive** to RDS/pgvector/EFS/S3/app compute.

**Cost-aware path:** the cheapest graph store is the RDS Postgres you ALREADY pay for —
`nodes`/`edges` tables + **recursive CTE** cost **~$0 incremental** and unify graph+vectors in
one store (no dual-store drift). At 100k nodes that's tiny for Postgres. So:
**start on Postgres-CTE; move to Neptune only when the milestone-4 spike shows CTE traversal
latency is insufficient at depth 6–8.** The `GraphStore` abstraction makes the swap painless;
Neptune remains the compliant scale-up target, paid for only when performance forces it.

## Pluggable graph storage (`GraphStore` port + adapters)

Ports & adapters: the engine and MCP tools talk only to a `GraphStore` port of **domain
operations** (not raw queries); each graph DB is an adapter. Key simplification: **Neptune,
Neo4j, FalkorDB all speak (open)Cypher** → 3 of 4 adapters share one Cypher impl; only
Postgres-CTE is separate.

```rust
#[async_trait] trait GraphStore: Send + Sync {
  // writes
  async fn ensure_schema(&self) -> Result<()>;
  async fn bulk_load(&self, a: &GraphArtifacts) -> Result<LoadStats>;
  async fn upsert_incremental(&self, d: &GraphDelta) -> Result<()>;
  async fn swap_version(&self, v: VersionId) -> Result<()>;        // atomic publish
  // reads — MCP tools map 1:1
  async fn get_node(&self, id: &NodeId) -> Result<Option<Node>>;
  async fn neighbors(&self, id: &NodeId, dir: Direction, kinds: &[EdgeKind]) -> Result<Vec<Edge>>;
  async fn impact(&self, id: &NodeId, dir: Direction, max_depth: u32) -> Result<Impact>;
  async fn call_chain(&self, from: &NodeId, to: &NodeId, max_depth: u32) -> Result<Vec<Path>>;
  async fn subgraph(&self, seeds: &[NodeId], radius: u32) -> Result<Subgraph>;  // GraphRAG
  async fn context(&self, id: &NodeId) -> Result<SymbolContext>;
}
```

- **`CypherGraphStore`** (shared) = domain→Cypher templates, parameterized by:
  - `CypherDriver` (`execute(query, params) → rows`): Neptune (HTTPS+SigV4), Neo4j (Bolt), FalkorDB (Redis).
  - `CypherDialect`: the few syntax deltas (var-length path, fn names).
- **`PostgresGraphStore`**: separate impl, recursive CTE over `nodes`/`edges`.
- **`BulkLoader`** is a separate port (load paths differ): `NeptuneLoader` (CSV→S3→loader API),
  `Neo4jLoader` (Aura bulk import), `FalkorLoader` (bulk tool), `PostgresLoader` (`COPY FROM STDIN`).
- Engine always emits **one canonical artifact** (`nodes.parquet` + `edges.parquet`, stable
  schema); each loader transforms to its backend format → engine stays backend-agnostic.
- Selection: `CIH_GRAPH_BACKEND=postgres|neptune|neo4j|falkor` via a factory. Swapping backends
  never touches the engine or MCP tools — makes the milestone-4 spike a config flip and guards
  against vendor death (the Kuzu lesson). (Same design in TS if the MCP server stays TS:
  `neo4j-driver` / Neptune-openCypher-HTTP / `falkordb` / `pg`.)

## Embeddings (transformers.js sidecar, or Rust fastembed)

`all-MiniLM-L6-v2` (384) or `bge-m3` (1024, stronger/multilingual). With a Rust MCP service,
either run embeddings **in Rust** (`fastembed`/`ort`) to unify, or keep a small **TS/Python
sidecar** that consumes the engine's `chunk-specs` and writes vectors to pgvector. pgvector
column dim must match the model. Opt-in / configurable (like GitNexus).

## Search & retrieval (query-time)

- **Hybrid search**: pgvector (semantic) + keyword + RRF (`k=60`). Keyword-arm decision:
  - **Postgres native FTS** (`tsvector`/`ts_rank`) — stateless, RDS-native, scales multi-tenant
    (recommended start). Note: true-BM25 extensions (ParadeDB `pg_search`) are **not available
    on RDS**, so "BM25 in Rust" lives at *build time* and/or as an in-process Rust scorer.
  - **Rust BM25 scorer** loaded in the API process from `bm25.bin` — truest to BM25, but stateful
    memory per repo. Add only if FTS ranking quality proves insufficient.
- **GraphRAG**: hybrid-search seed nodes → expand subgraph via recursive CTE → assemble
  code + neighbors → LLM chat (Next.js). Wiki pages auto-generated from communities/processes.

## Product surface & orchestration

- **MCP server** (Rust `rmcp` + axum, **Streamable HTTP**/JSON-RPC) for editor agents
  (Claude Code/Cursor) — they bring their own orchestration; CIH just serves tools. `rmcp` verdict:
  production-ready (official, stable v1.x; pin the version, it iterates fast).
- **CIH chat product**: **Claude Agent SDK (TS)** in the Next.js BFF runs the agentic GraphRAG
  loop — seed via hybrid search, then let Claude call CIH's MCP tools (`impact`/`context`/
  `subgraph`) to traverse further. Models: Opus 4.8 (deep) / Sonnet 4.x (cost-sensitive).
- **Next.js** chat + **Docusaurus** wiki auto-rebuilt from the graph.
- **AWS**: EFS (decompiled source + wiki), S3 (bulk-load artifacts), RDS Postgres (pgvector),
  FalkorDB→Neptune (graph).

## Milestones

1. **Boundary first**: `cih-engine` napi skeleton that returns dummy COPY artifacts; wire into a
   minimal TS harness (prove async + file handoff end-to-end).
2. **Java parse + structure** in Rust → `nodes/edges` → Postgres COPY; index a small repo.
3. **Scope-res + MRO** in Rust → CALLS edges; `service.save()` resolves correctly.
4. **Graph store (FalkorDB) + spike**: `impact`/`context` via `GraphStore` on **FalkorDB** (dev);
   run the spike (FalkorDB vs Neptune vs Postgres-CTE) at depth 2/4/6/8. Neptune is the go-live target.
5. **Communities (Leiden) + processes + chunk specs**; transformers.js embeddings + pgvector;
   hybrid search + RRF.
6. **Fernflower** integration for dependency JARs.
7. **Spring pre-phase** (stereotypes/routes/JPA tags). DI-aware interface→impl resolution = later.
8. **Product shell**: `rmcp` Streamable HTTP endpoint, Claude Agent SDK GraphRAG chat in Next.js,
   Docusaurus rebuild; AWS deploy. Graph backend flip FalkorDB → Neptune at go-live.

## Open decisions / spikes

- Graph store: **Amazon Neptune (recommended) vs Neo4j Aura vs Postgres recursive-CTE** (milestone 4 spike). Kuzu eliminated (abandoned / Apple acquisition, Oct 2025).
- BM25 location: Postgres FTS (default) vs in-process Rust scorer.
- Embedding model + pgvector dim: MiniLM-384 vs bge-m3-1024.
- napi data transfer: **file-based handoff** (recommended) vs buffers.

## Risks

- **FFI copy cost** if large arrays cross napi — mitigated by file-based handoff.
- **Deep-traversal latency** on long call chains (Neptune per-query HTTP overhead; Postgres CTE depth) — the milestone-4 spike de-risks this; cache hot subgraphs.
- **Dual-store drift** (pgvector + graph DB) — bulk-load both from the same artifacts, version/swap atomically.
- **Graph-DB vendor risk** — Kuzu's death is the cautionary tale; Neptune (AWS) is lowest-risk, FalkorDB highest.
- **RDS extension limits**: pgvector ✓; ParadeDB/`pg_search` BM25 ✗ on RDS → keyword arm must be
  Postgres FTS or Rust.
- **Decompiled-code noise** and the **resolution edge-case corpus** (the real moat) — start
  Java-only, grow cases from the real Spring repo; mirror `test/integration/resolvers/java.test.ts`.
- **Multi-tenant memory** if BM25 runs in-process.

## Verification

- **Correctness**: index `spring-petclinic`; confirm interface-call resolution; compare
  node/edge counts and `impact`/`context` results against GitNexus on the same repo.
- **Performance**: index the real large repo (~12.7k files / 100k fns + decompiled deps); record
  per-phase timing + peak RSS; compare the Rust core against a TS-worker baseline. Target ≥2×
  lower peak memory and resolution no longer single-threaded.
- **Traversal latency**: CTE depth tests (impact at depth 2/4/6/8) feeding the storage decision.
