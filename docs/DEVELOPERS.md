# Developer guide — how yummy-cih works

New to the codebase? Start here. This explains **what the system does, how a repo
becomes a queryable graph, which crate owns each step, and where to start reading.**
For *using* the tool (indexing a repo, the MCP tools, config) see the
[README](../README.md). For parser assumptions and known limits see
[ARCHITECTURE.md](ARCHITECTURE.md). For unfamiliar terms see
[glossary.md](glossary.md).

## The one-paragraph mental model

CIH (Code Intelligence Hub) turns a source repository into a **graph of code
symbols** (classes, methods, routes, DB tables…) connected by **relationships**
(calls, imports, handles-route, reads-table…). It parses source with tree-sitter,
resolves references into edges, stores the graph in FalkorDB, detects feature
communities, and serves structural questions to an LLM over an **MCP server**
(`impact`, `trace_flow`, `route_map`, …). It is language-agnostic (Java, Kotlin,
TypeScript/JS, Python, Go, …) and storage-agnostic (FalkorDB today, Neptune later).

## The pipeline

Everything flows one direction. The `cih-engine` binary orchestrates these stages;
each writes something the next consumes.

```
 source files
     │  scan        pick which files/modules to index
     ▼
   PARSE            tree-sitter → per-file IR (nodes + unresolved references)
     │              cih-lang · cih-parse · cih-jar
     ▼
   RESOLVE          turn references into real edges (calls, imports, types)
     │              cih-resolve
     ▼
   .cih/artifacts   nodes.jsonl + edges.jsonl  (the canonical graph on disk)
     │              cih-core
     ▼
   LOAD             stream the artifacts into the graph database
     │              cih-graph-store (port) → cih-falkor (FalkorDB adapter)
     ▼
   DISCOVER         communities (feature modules) + cross-repo contracts
     │              cih-community · cih-grouping
     ▼
   EMBED / SEARCH   semantic (pgvector) + lexical (BM25) indexes
     │              cih-embed · cih-search
     ▼
   WIKI             generate human-readable docs from the graph
                    cih-wiki
```

Then the **MCP server** (`cih-server`) answers questions by querying the loaded
graph, and **taint analysis** (`cih-taint`) walks source→sink flows for security.

## Which crate owns what

| Stage / role | Crate(s) | Responsibility |
|---|---|---|
| **Vocabulary** | `cih-core` | The domain types everything shares: `NodeId`, `Node`, `Edge`, `NodeKind`/`EdgeKind`, `ParsedUnit`, `GraphArtifacts` (the JSONL read/write), plus the repo/group registries. Read this first. |
| **Parse** | `cih-lang` | One tree-sitter parser per language (a `LanguageProvider`) that turns a source file into IR: nodes + *unresolved* references. |
| **Parse** | `cih-parse` | The parse driver/registry — dispatches each file to the right `LanguageProvider` and collects the IR. |
| **Parse (deps)** | `cih-jar` | Signature-only API extraction from `.jar`/`.class` bytecode, so app→library calls resolve to real nodes. |
| **Resolve** | `cih-resolve` | Builds cross-file indexes and turns unresolved references into real edges (calls, imports, type refs, route handlers). The heart of graph accuracy. |
| **Store (port)** | `cih-graph-store` | The storage-agnostic `GraphStore` trait — *domain* operations, not raw queries. The engine and server talk only to this. |
| **Store (adapter)** | `cih-falkor` | The FalkorDB implementation: Cypher queries, the native `GRAPH.BULK` loader, and the flow/impact traversals. |
| **Discover** | `cih-community` | Community detection (Leiden clustering) + process tracing — groups symbols into feature modules. |
| **Discover (cross-repo)** | `cih-grouping` | Groups repos and matches producer↔consumer **contracts** (HTTP routes, events) across services. |
| **Search** | `cih-embed` | Semantic embeddings of nodes (stored in pgvector) for meaning-based search. |
| **Search** | `cih-search` | Lightweight lexical BM25 search over nodes (storage-free). |
| **Security** | `cih-taint` | Inter/intra-procedural taint analysis: source→sink flows (SQLi, exec, XSS…). |
| **Extensibility** | `cih-patterns` | User-defined *resolve patterns* — teach CIH a codebase's own framework conventions without new hardcoded handlers. |
| **Docs** | `cih-wiki` | Generates the human-readable wiki (per-module pages, route maps) from the graph. |
| **Serve** | `cih-server` | The MCP server (`rmcp` + `axum`, streamable HTTP): the tools an LLM calls — `query`, `context`, `impact`, `trace_flow`, `route_map`, `taint_paths`, … |
| **Orchestrate** | `cih-engine` | The CLI binary. Wires the whole pipeline, resolves config, writes `.cih/` artifacts, loads the store. Commands live in `crates/cih-engine/src/cmd/`. |

Both binaries — `cih-engine` (CLI) and `cih-server` (MCP) — are **thin shims** over
their library crates (`cih_engine` / `cih_server`).

## Adding a cih-server capability

Follow the existing request path:

1. Add the protocol DTO to `transport/mcp/args.rs`.
2. Add a validated command and typed output to the owning `application`
   capability.
3. Depend on a `ports` trait when the use case needs files, processes,
   persistence, registry data, or another external system.
4. Implement that port under `infrastructure` and wire it once in
   `bootstrap::assemble_services`.
5. Keep the MCP or HTTP adapter limited to conversion, one application call,
   and response/error mapping.
6. Add direct application tests, serialization contract coverage, and adapter
   dispatch coverage when the tool surface changes.

Do not import `rmcp` or Axum outside `transport`, and do not import
`infrastructure` from production `application` code. The
`architecture_boundaries` integration test enforces these rules.

## Suggested reading order

To understand the concepts without drowning, read in dependency order — each builds
on the last:

1. **`cih-core/src/lib.rs`** — the vocabulary (`NodeId`, `Node`, `Edge`, kinds,
   `ParsedUnit`, `GraphArtifacts`). Everything below speaks these types.
2. **One parser in `cih-lang`** — e.g. the Go or Python provider — to see a source
   file become nodes + unresolved references.
3. **`cih-resolve/src/index.rs` + `emit.rs`** — how references become edges.
4. **`cih-falkor/src/lib.rs`** (+ `bulk.rs`) — how the graph is loaded and queried.
5. **`cih-server/src/transport/mcp/tools/graph.rs`** and
   **`cih-server/src/application/graph/mod.rs`** — how a tool call becomes a
   validated application command, graph query, and typed answer.
6. **`cih-engine/src/cmd/analyze.rs`** — how it's all wired end-to-end.

## Where things live

- **Source:** `crates/<crate>/src/` — each crate's `lib.rs` opens with a `//!`
  overview and is a *map* of its modules.
- **Config (per target repo, at its root):** `cih.toml` (options), `cih.scope.toml`
  (what to index), `cih.taint.toml` (taint rules) — see the README.
- **Docs:** `docs/` — `ARCHITECTURE.md` (parser assumptions + graph limits),
  `SECURITY.md`, `glossary.md`, `agent-workflows/` (persona playbooks),
  `runbooks/`, `plans/` (active design docs), `archive/` (finished history).
- **Contributing:** [CONTRIBUTING.md](../CONTRIBUTING.md) — the module/naming
  standard and the build/test/lint gates.

## Build & run (dev)

```bash
cargo build                       # whole workspace
cargo test --workspace            # hermetic — no FalkorDB/Postgres needed
# local services when you do need them:
#   FalkorDB on 6380 (Homebrew redis squats 6379), Postgres on 5433
FALKOR_URL=redis://127.0.0.1:6380 cargo run -p cih-engine -- analyze /path/to/repo
```

See the README for the full user workflow and the MCP server.
