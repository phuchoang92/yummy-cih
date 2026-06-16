# CIH — Code Intelligence Hub

An MCP server that indexes Java/Spring codebases into a graph, then answers questions about
structure, call chains, DB access, routes, and more.

---

## Quick Start (Docker Compose)

### 1. Pull images

```bash
docker compose pull
```

### 2. Start the stack

```bash
docker compose up -d
```

This starts two services:

| Service | Port | Purpose |
|---|---|---|
| `falkordb` | 6380 (host) | Graph database (persisted in `falkordb-data` volume) |
| `cih-server` | 8080 (host) | MCP server (tools: context, impact, search) |

### 3. Tell compose where your Java project is

Create a `.env` file next to `docker-compose.yml`:

```bash
# .env
REPO_PATH=/absolute/path/to/your/java-project
```

### 4. Index your Java project

```bash
# Parse + resolve — writes artifacts into <repo>/.cih/
docker compose run --rm engine analyze /repo --all

# Community detection — groups related classes into modules
docker compose run --rm engine discover /repo

# Generate wiki docs (PO / BA / Dev role pages)
docker compose run --rm engine wiki /repo --out /repo/.cih/wiki
```

Each command exits when done. The stack (`cih-server` + `falkordb`) keeps running in the background.

### 5. View the wiki

The wiki is written as Markdown to `<your-repo>/.cih/wiki/pages/`. Open the files in any
Markdown viewer or serve them locally:

```bash
# Quick preview with Python
cd /path/to/your/java-project/.cih/wiki
python3 -m http.server 3000
# open http://localhost:3000/pages/po/index.md
```

Role pages generated:

| Role | Path | Contents |
|---|---|---|
| PO | `pages/po/` | Routes, business processes, core DB tables per module |
| BA | `pages/ba/` | Workflow steps, inter-module call flows, data access matrix |
| Dev | `pages/dev/` | Classes, stereotypes, routes, DB access, test coverage |
| Shared | `pages/shared/` | Full API route list (Markdown + OpenAPI JSON) |

### 6. Connect Claude / MCP Inspector

The MCP server listens on `http://localhost:8080/mcp` (Streamable HTTP / JSON-RPC).

**MCP Inspector:**
```bash
npx @modelcontextprotocol/inspector
# Connect to: http://localhost:8080/mcp
```

**Claude Desktop** — add to `claude_desktop_config.json`:
```json
{
  "mcpServers": {
    "cih": {
      "command": "curl",
      "args": ["-s", "http://localhost:8080/mcp"]
    }
  }
}
```

---

## Environment Variables

All variables have defaults baked into the Docker image:

| Variable | Default | Meaning |
|---|---|---|
| `FALKOR_URL` | `redis://falkordb:6379` | FalkorDB connection (use service name inside compose) |
| `CIH_GRAPH_KEY` | `cih` | Graph name in FalkorDB |
| `CIH_BIND` | `0.0.0.0:8080` | MCP server listen address |
| `CIH_ARTIFACTS_DIR` | `/data/artifacts` | Where analyze output is stored inside the container |
| `HF_HOME` | `/data/hf-cache` | HuggingFace model cache (embedding model downloaded on first use) |

Override in `docker-compose.yml` under the `cih-server → environment` section.

---

## Data Persistence

Two Docker volumes are created automatically:

| Volume | Mounted at | Contains |
|---|---|---|
| `falkordb-data` | FalkorDB container | Graph data (survives restarts) |
| `cih-data` | cih-server `/data` | Artifacts + embedding model cache |

To wipe and start fresh:
```bash
docker compose down -v   # removes volumes too
```

---

## Stopping

```bash
docker compose down
```

---

## Local Development Build

Only needed if you want to modify the engine:

```bash
# Prerequisites: Rust stable, Docker (for FalkorDB)
cargo build --release -p cih-server -p cih-engine

# Run FalkorDB on port 6380 (brew redis uses 6379)
docker compose up -d falkordb

# Run the MCP server locally
FALKOR_URL=redis://localhost:6380 CIH_GRAPH_KEY=cih \
  ./target/release/cih-server

# Index a project locally
FALKOR_URL=redis://localhost:6380 CIH_GRAPH_KEY=cih \
  ./target/release/cih-engine analyze /path/to/repo --all
```

Run tests:
```bash
cargo test --workspace
```

---

## Workspace Layout

```
yummy-cih/
├─ crates/
│  ├─ cih-core/        Domain types (NodeId, NodeKind, EdgeKind, Node, Edge, IR)
│  ├─ cih-parse/       Java tree-sitter parser → ParsedFile IR + SQL scanner
│  ├─ cih-resolve/     Edge resolver (DI-aware), DB access emitter
│  ├─ cih-falkor/      FalkorDB adapter (openCypher over Redis protocol)
│  ├─ cih-search/      BM25 + embedding search
│  ├─ cih-wiki/        Wiki renderer (PO / BA / Dev role pages)
│  └─ cih-engine/      CLI: analyze · discover · wiki commands
│  └─ cih-server/      MCP server (rmcp + axum, Streamable HTTP)
├─ docker-compose.yml
├─ Dockerfile
└─ ROADMAP.md
```
