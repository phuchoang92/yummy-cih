# CIH — Code Intelligence Hub (Rust engine + MCP service)

Milestone-1 scaffold: a Rust `rmcp` + `axum` **MCP server** over **Streamable HTTP**, backed by a
**pluggable `GraphStore`** with a **FalkorDB** adapter (the open-source / dev backend). At go-live
the same openCypher queries move to an Amazon Neptune adapter via a `CIH_GRAPH_BACKEND` flip.

See `../cih-plan.md` for the full architecture and `../high-architecture` for the diagram.

## Workspace layout

```
yummy-cih/
├─ crates/
│  ├─ cih-core/         domain types (NodeId, NodeKind, EdgeKind, Node, Edge, GraphArtifacts)
│  ├─ cih-graph-store/  the GraphStore + BulkLoader ports + domain query types  ← the abstraction
│  ├─ cih-falkor/       FalkorDB adapter (openCypher over the Redis protocol)
│  └─ cih-server/      rmcp + axum MCP server (tools: context, impact)
└─ Cargo.toml          workspace
```

`cih-core`, `cih-graph-store`, `cih-falkor` are pure Rust and compile-ready. `cih-server` targets a
recent `rmcp`; see the version note below.

## Prerequisites

- Rust (stable) via [rustup](https://rustup.rs).
- A running FalkorDB:

```bash
docker run -p 6379:6379 -it falkordb/falkordb:latest
```

## Run

```bash
cd yummy-cih
# defaults: backend=falkor, bind=127.0.0.1:8080, FALKOR_URL=redis://127.0.0.1:6379, graph_key=cih
cargo run -p cih-server
```

Environment variables:

| Var | Default | Meaning |
|-----|---------|---------|
| `CIH_GRAPH_BACKEND` | `falkor` | `falkor` \| `neptune` \| `postgres` |
| `CIH_BIND` | `127.0.0.1:8080` | listen address |
| `FALKOR_URL` | `redis://127.0.0.1:6379` | FalkorDB (Redis protocol) URL |
| `CIH_GRAPH_KEY` | `cih` | FalkorDB graph name |

## Seed a tiny graph + try a tool

Seed two nodes and a CALLS edge directly in FalkorDB:

```bash
redis-cli GRAPH.QUERY cih "CREATE (a:Symbol {id:'Method:UserController#register', name:'register'})-[:CALLS]->(b:Symbol {id:'Method:UserService#save', name:'save'})"
```

The MCP endpoint is `POST http://127.0.0.1:8080/mcp` (Streamable HTTP / JSON-RPC). Easiest way to
exercise it is the MCP Inspector:

```bash
npx @modelcontextprotocol/inspector
# connect to: http://127.0.0.1:8080/mcp  → call `impact` with {"name":"Method:UserService#save"}
```

You should see `register` returned as an upstream (caller) of `save`.

## rmcp version note (important)

`rmcp` iterates fast. The `#[tool_router]` / `#[tool]` / `ServerHandler` macros and the
`StreamableHttpService::new(...)` signature can differ between releases. If `cargo build` flags the
wiring in `crates/cih-server/src/main.rs`, reconcile **only that wiring** against
<https://docs.rs/rmcp> for the version you resolve — the tool bodies (the `self.store.*` calls) and
the entire `cih-*` stack are SDK-agnostic and unchanged. Pin the exact version in `Cargo.toml` once
it builds.

## What's stubbed (next milestones)

- `bulk_load` / `upsert_incremental` / `swap_version` on the FalkorDB adapter → the **BulkLoader**
  milestone (UNWIND batches; later Neptune S3-CSV loader).
- `call_chain` / `subgraph` parse FalkorDB replies via scalar columns only; full result parsing
  uses the compact protocol later.
- Switch the inline-escaped queries in `cih-falkor` to **FalkorDB query parameters** before prod.
- Adapters: `cih-neptune` (go-live) and `cih-postgres` (recursive-CTE, ~$0 fallback).
- The engine itself (parse → scope-res → MRO → graph → Leiden → BM25) lands behind this server and
  produces the canonical `GraphArtifacts` the BulkLoader consumes.
