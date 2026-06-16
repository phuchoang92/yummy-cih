# CIH — Code Intelligence Hub

An MCP server that indexes Java/Spring codebases into a graph database, then answers
architecture questions about structure, call chains, routes, DB access, communities, and more.
It also generates a role-based wiki (PO / BA / Dev pages) from those artifacts — with optional
LLM enrichment.

---

## Prerequisites

- **Docker** and **Docker Compose** (v2)
- **Node.js ≥ 18** — only needed to run the docs viewer
- A Java/Spring repository on your local machine

---

## Quick Start (Docker Compose)

### 1. Configure your repo path

Create a `.env` file next to `docker-compose.yml`:

```bash
REPO_PATH=/absolute/path/to/your/java-project
```

This is the only required variable. Both the `engine` and `cih-server` containers mount this
path, so the indexer and the MCP server read from the same artifact files.

### 2. Start FalkorDB + the MCP server

```bash
docker compose pull
docker compose up -d
```

| Service | Host port | Purpose |
|---|---|---|
| `falkordb` | 6380 | Graph database (persisted in `falkordb-data` volume) |
| `cih-server` | 8080 | MCP server — `context`, `impact`, `trace_flow`, `query`, and more |

Wait until healthy:
```bash
docker compose ps   # both should show "running" / "healthy"
```

### 3. (Optional) Scan first — recommended for large repos

```bash
docker compose run --rm engine scan /repo
```

Prints a module breakdown and recommended scope without touching the database. Use this to
decide which modules to index before committing to a full parse.

### 4. Index your project

Run these three commands in order. Each exits when done.

```bash
# Parse + resolve Java source → writes .cih/artifacts/ inside your repo
docker compose run --rm engine analyze /repo --all

# Community detection → groups classes into feature modules
docker compose run --rm engine discover /repo

# Embedding index → enables semantic search in the query tool (optional)
docker compose run --rm engine embed /repo
```

> **Scoping large repos:** `--all` indexes every Java file. For a repo with thousands of
> files, use `--module payment,order` to index specific modules only, or drop a
> `cih.scope.toml` at the repo root (see [Scoping](#scoping-large-repos) below).

### 5. Generate wiki docs

```bash
docker compose run --rm engine wiki /repo
```

Markdown pages are written to `$REPO_PATH/.cih/wiki/pages/` in a feature-first layout:

```
pages/
  index.md                    ← system overview (all features, route/module counts)
  routes.md                   ← full API route list
  <feature>/
    index.md                  ← feature landing page
    po.md                     ← business overview (routes, tables, processes)
    ba.md                     ← workflow analysis (call flows, events, data access)
    dev/
      <class-name>.md         ← technical reference per module (classes, routes, DB)
```

**With LLM enrichment** — adds AI-generated summaries to every page:

```bash
# OpenAI
docker compose run --rm engine wiki /repo \
  --llm --llm-provider openai-compatible \
  --llm-model gpt-4o-mini \
  --llm-base-url https://api.openai.com/v1

# Anthropic
docker compose run --rm engine wiki /repo \
  --llm --llm-provider anthropic \
  --llm-model claude-haiku-4-5-20251001 \
  --llm-base-url https://api.anthropic.com/v1

# Local Ollama
docker compose run --rm engine wiki /repo \
  --llm --llm-provider http-json \
  --llm-provider-config /repo/ollama.json
```

Set the API key in your shell before running:
```bash
export CIH_LLM_API_KEY=sk-...
```

### 6. View the wiki in a browser

The `docs-viewer` is a Docusaurus site in this repo. It works with any repo's wiki output.

```bash
cd docs-viewer
npm install          # first time only
CIH_WIKI_PATH=/absolute/path/to/your/java-project/.cih/wiki/pages npm start
```

Opens at **http://localhost:3001**. The sidebar is auto-generated from the feature folder
structure. The repo name is read from `manifest.json`.

To switch to a different repo later, just re-run with a different `CIH_WIKI_PATH`.

### 7. Connect to the MCP server

The server listens on `http://localhost:8080/mcp` (Streamable HTTP / JSON-RPC).

**MCP Inspector** — quickest way to test all tools:
```bash
npx @modelcontextprotocol/inspector
# URL: http://localhost:8080/mcp
```

**Claude Code CLI:**
```bash
claude mcp add --transport http cih http://localhost:8080/mcp
```

**Claude Desktop** — add to `claude_desktop_config.json`:
```json
{
  "mcpServers": {
    "cih": {
      "command": "npx",
      "args": ["-y", "mcp-remote", "http://localhost:8080/mcp"]
    }
  }
}
```

Available MCP tools:

| Tool | Personas | What it answers |
|---|---|---|
| `context` | All | Classes, methods, routes for a symbol |
| `impact` | Dev | Upstream callers + blast radius of a change |
| `trace_flow` | PO, BA | End-to-end execution chain from a route or method |
| `feature_map` | PO, BA | Map a business keyword to code communities |
| `query` | All | Hybrid BM25 + semantic search over the graph |
| `route_map` | PO | All HTTP routes, filterable by prefix |
| `communities` | All | Detected feature modules with cohesion scores |
| `test_coverage` | Tester | Test classes covering a symbol |
| `regression_scope` | Tester | Tests to re-run for a set of changed files |
| `detect_changes` | Dev | Changed symbols + their blast radius (git-aware) |
| `group_contracts` | Architect | Cross-service HTTP + event contracts for a repo group |

---

## Scoping Large Repos

`--all` indexes every Java file. For projects with decompiled dependencies or generated
code, scope the analysis to the modules you own:

```bash
# Index specific modules by folder name
docker compose run --rm engine analyze /repo --module payment,order,auth

# Include specific globs
docker compose run --rm engine analyze /repo --include "src/main/java/com/example/**"

# Exclude generated or decompiled dirs
docker compose run --rm engine analyze /repo --all --exclude ".workspace-dependencies/**"
```

For a persistent scope, drop a `cih.scope.toml` at the repo root:

```toml
# cih.scope.toml
modules = ["payment", "order", "auth", "product"]
exclude = [".workspace-dependencies", "src/test"]
```

Then just run `analyze /repo` without extra flags — the scope file is picked up automatically.

---

## Environment Variables

| Variable | Default | Meaning |
|---|---|---|
| `FALKOR_URL` | `redis://falkordb:6379` | FalkorDB connection (service name inside compose) |
| `CIH_GRAPH_KEY` | `cih` | Graph name in FalkorDB |
| `CIH_BIND` | `0.0.0.0:8080` | MCP server listen address |
| `CIH_ARTIFACTS_DIR` | `/repo/.cih/artifacts` | Artifact path for BM25 `query` tool |
| `HF_HOME` | `/data/hf-cache` | HuggingFace model cache (downloaded on first `embed`) |
| `CIH_LLM_API_KEY` | — | API key for `wiki --llm` (falls back to `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`) |

Override any variable under `cih-server → environment` in `docker-compose.yml`.

---

## Data Persistence

| Volume | Mounted at | Contains |
|---|---|---|
| `falkordb-data` | FalkorDB container | Graph data — survives restarts |
| `cih-data` | `/data` in cih-server | Embedding model cache |
| *(your repo)* | `/repo` in both containers | Source files + `.cih/` artifacts |

Wipe graph and start fresh:
```bash
docker compose down -v   # removes both volumes
```

Stop without wiping:
```bash
docker compose down
```

---

## Troubleshooting

**Port 6380 already in use**

Homebrew installs Redis on port 6379 by default, but CIH maps FalkorDB to 6380. If something
else is on 6380, change the host port in `docker-compose.yml`:

```yaml
falkordb:
  ports:
    - "6381:6379"   # use 6381 on the host instead
```

**`analyze` fails on some files**

The parser skips files it cannot read and continues. Check the output for `parse errors: N`.
If a module is consistently failing, use `--exclude` to skip it. Run with `RUST_LOG=debug`
for per-file detail:

```bash
RUST_LOG=debug docker compose run --rm engine analyze /repo --all
```

**`wiki` command says "no community artifacts"**

Run `discover` before `wiki`. The wiki command requires community detection output:
```bash
docker compose run --rm engine discover /repo
docker compose run --rm engine wiki /repo
```

**`query` tool returns no results**

BM25 search reads artifacts from `CIH_ARTIFACTS_DIR`. The default config points to
`/repo/.cih/artifacts` inside the server container. If you changed `REPO_PATH` after the
initial stack start, restart the server so it picks up the new mount:

```bash
docker compose restart cih-server
```

Semantic (embedding) search also requires `embed` to have been run:
```bash
docker compose run --rm engine embed /repo
```

**`docker compose run --rm engine` does nothing / service not found**

The `engine` service has `profiles: ["tools"]` so `docker compose up` intentionally skips it
(it's a one-shot runner, not a daemon). Use `docker compose run --rm engine <command>` to
invoke it — this works regardless of profiles.

**Re-index after source changes**

Just re-run `analyze` and `discover`. The engine uses content-addressed caching, so unchanged
files are skipped automatically. Force a full re-parse with `--no-cache`.

---

## Local Development Build

Only needed to modify the engine or server:

```bash
# Prerequisites: Rust stable, Docker (for FalkorDB)

# Start FalkorDB only
docker compose up -d falkordb

# Build
cargo build --release -p cih-server -p cih-engine

# Run the MCP server
FALKOR_URL=redis://localhost:6380 CIH_GRAPH_KEY=cih \
  CIH_ARTIFACTS_DIR=/path/to/repo/.cih/artifacts \
  ./target/release/cih-server

# Index a project
FALKOR_URL=redis://localhost:6380 CIH_GRAPH_KEY=cih \
  ./target/release/cih-engine analyze /path/to/repo --all

./target/release/cih-engine discover /path/to/repo
./target/release/cih-engine wiki /path/to/repo

# Run all tests
cargo test --workspace
```

---

## Workspace Layout

```
yummy-cih/
├─ crates/
│  ├─ cih-core/          Domain types: NodeId, NodeKind, EdgeKind, Node, Edge, IR
│  ├─ cih-lang/          Language provider trait (Java implementation)
│  ├─ cih-parse/         Java tree-sitter parser → ParsedFile IR + SQL scanner
│  ├─ cih-resolve/       Scope resolver (DI-aware, MRO, DB access emitter)
│  ├─ cih-jar/           JAR API-surface extractor (signature-only, no decompiler)
│  ├─ cih-community/     Leiden-style community detection + BFS process tracing
│  ├─ cih-graph-store/   GraphStore + BulkLoader trait definitions
│  ├─ cih-falkor/        FalkorDB adapter (openCypher over Redis protocol)
│  ├─ cih-search/        BM25 tokenizer + Reciprocal Rank Fusion
│  ├─ cih-embed/         Embedding chunker + fastembed + pgvector index
│  ├─ cih-wiki/          Wiki renderer: WikiGraph, feature-first page hierarchy
│  ├─ cih-engine/        CLI: scan · analyze · discover · embed · wiki · group
│  └─ cih-server/        MCP server (rmcp + axum, Streamable HTTP)
├─ docs-viewer/          Docusaurus viewer for any repo's wiki output
├─ docker-compose.yml
├─ Dockerfile
└─ ROADMAP.md
```
