# yummy-cih Usage Guide

This guide explains how to use `yummy-cih` to scan a Java/Spring repository, parse selected code, build graph artifacts, load them into FalkorDB, and query the result through the MCP server.

## What yummy-cih does

`yummy-cih` is a code intelligence pipeline for Java repositories.

At a high level it:

1. Scans a repository and creates a fast module/file map.
2. Selects a scope of Java files to analyze.
3. Parses Java files into structured intermediate data.
4. Builds graph nodes and edges for code entities and relationships.
5. Optionally loads that graph into FalkorDB.
6. Optionally detects communities and process traces.
7. Optionally embeds searchable graph nodes into pgvector.
8. Serves MCP tools so an assistant can ask codebase questions.

The main crates you use directly are:

- `cih-engine`: command-line scanner, analyzer, resolver, discovery, and embedding tool.
- `cih-server`: MCP server that reads the graph and artifacts.

## Prerequisites

Install:

- Rust toolchain with `cargo`.
- Docker, if you want to run FalkorDB locally.
- Optional: PostgreSQL with pgvector, if you want semantic search.
- Optional: MCP Inspector or another MCP client, if you want to test the server manually.

From the project root:

```bash
cd /Users/phuc/BigMoves/AI/yummy-cih
cargo test --workspace
cargo run -p cih-engine -- --help
```

## Start FalkorDB

The engine defaults to `redis://127.0.0.1:6380`, while the server default is `redis://127.0.0.1:6379`. To avoid confusion, set `FALKOR_URL` explicitly for both.

```bash
docker rm -f falkordb 2>/dev/null || true
docker run -d --name falkordb -p 6380:6379 falkordb/falkordb:latest

export FALKOR_URL=redis://127.0.0.1:6380
export CIH_GRAPH_KEY=cih
```

## Recommended Workflow

Set the repository you want to analyze:

```bash
export REPO=/absolute/path/to/java-repository
```

Run the fast scan:

```bash
cargo run -p cih-engine -- scan "$REPO"
```

Preview analysis without loading FalkorDB:

```bash
cargo run -p cih-engine -- analyze "$REPO" --all --no-load
```

Analyze and load the graph:

```bash
cargo run -p cih-engine -- analyze "$REPO" --all
```

Run community and process discovery:

```bash
cargo run -p cih-engine -- discover "$REPO"
```

Start the MCP server:

```bash
export CIH_ARTIFACTS_DIR="$REPO/.cih/artifacts"
cargo run -p cih-server
```

The MCP endpoint is:

```text
http://127.0.0.1:8080/mcp
```

The built-in local graph browser is:

```text
http://127.0.0.1:8080/graph
```

Use it to search symbols, inspect context, render impact graphs, trace flows,
view communities, and browse indexed routes while the full `yummy` frontend is
being developed.

## LLM Enrichment

The `wiki` command can call an LLM to generate richer documentation (descriptions, business summaries, feature grouping). See **[docs/llm-providers.md](docs/llm-providers.md)** for the full provider guide, including:

- Quick-start commands for DeepSeek, Gemini, Anthropic, OpenAI, and self-hosted models.
- API key environment variable names per provider.
- `--wiki-mode` options (`graph`, `llm-summary`, `llm-full`).

## CLI Commands

### Scan

`scan` performs a fast repository discovery pass. It does not parse Java syntax and does not write graph data.

```bash
cargo run -p cih-engine -- scan "$REPO"
```

Write machine-readable output to stdout:

```bash
cargo run -p cih-engine -- scan "$REPO" --json
```

Main output:

```text
$REPO/.cih/repo-map.json
```

The scan detects:

- Maven modules from `pom.xml`.
- Gradle modules from `settings.gradle`, `settings.gradle.kts`, `build.gradle`, and `build.gradle.kts`.
- Java files.
- Package declarations.
- Lightweight Spring signals.
- Ignored, generated, vendored, and `.workspace-dependencies/` paths.

### Analyze

`analyze` parses the selected Java files, emits graph artifacts, and optionally loads them into FalkorDB.

Analyze everything selected by scan:

```bash
cargo run -p cih-engine -- analyze "$REPO" --all
```

Analyze one or more modules:

```bash
cargo run -p cih-engine -- analyze "$REPO" --module app
cargo run -p cih-engine -- analyze "$REPO" --module app --module infra
cargo run -p cih-engine -- analyze "$REPO" --module app,infra
```

Analyze matching files:

```bash
cargo run -p cih-engine -- analyze "$REPO" \
  --include "src/main/java/**/*.java" \
  --exclude "**/*Test.java"
```

Preview without graph loading:

```bash
cargo run -p cih-engine -- analyze "$REPO" --all --no-load
```

Print JSON summary:

```bash
cargo run -p cih-engine -- analyze "$REPO" --all --json
```

Use a custom graph target:

```bash
cargo run -p cih-engine -- analyze "$REPO" --all \
  --falkor-url redis://127.0.0.1:6380 \
  --graph-key cih
```

Disable the parse cache:

```bash
cargo run -p cih-engine -- analyze "$REPO" --all --no-cache
```

Include decompiled workspace dependency files:

```bash
cargo run -p cih-engine -- analyze "$REPO" --all --include-decompiled
```

Main outputs:

```text
$REPO/.cih/scope.json
$REPO/.cih/file-hashes.json
$REPO/.cih/parse-cache/
$REPO/.cih/parsed/<version>/parsed-files.jsonl
$REPO/.cih/artifacts/<version>/nodes.jsonl
$REPO/.cih/artifacts/<version>/edges.jsonl
```

### Resolve

`resolve` reruns graph resolution using the saved scope in `.cih/scope.json`. Use this after resolver changes when you do not need to rescan or reselect scope.

```bash
cargo run -p cih-engine -- resolve "$REPO"
```

Preview without loading:

```bash
cargo run -p cih-engine -- resolve "$REPO" --no-load
```

Print JSON:

```bash
cargo run -p cih-engine -- resolve "$REPO" --json
```

### Discover

`discover` detects higher-level graph structure from the latest analyzed artifacts, including communities and process traces.

```bash
cargo run -p cih-engine -- discover "$REPO"
```

Preview without loading:

```bash
cargo run -p cih-engine -- discover "$REPO" --no-load
```

Print JSON:

```bash
cargo run -p cih-engine -- discover "$REPO" --json
```

Main outputs:

```text
$REPO/.cih/artifacts-community/<version>/
```

### Embed

`embed` creates vector embeddings for searchable graph nodes and stores them in PostgreSQL with pgvector.

Required environment variable:

```bash
export CIH_PG_URL=postgres://cih:changeme@localhost:5433/cih
```

Run with the default model:

```bash
cargo run -p cih-engine -- embed "$REPO"
```

Choose a model:

```bash
cargo run -p cih-engine -- embed "$REPO" --model all-minilm-l6-v2
cargo run -p cih-engine -- embed "$REPO" --model bge-small-en-v1.5
```

Print JSON:

```bash
cargo run -p cih-engine -- embed "$REPO" --json
```

The first embedding run can be slower because the model may need to download.

### Wiki

`wiki` renders a role-based documentation bundle from the graph artifacts produced by
`analyze`, `discover`, and (optionally) `embed`. Outputs Markdown pages to
`$REPO/.cih/wiki/pages/`.

Basic run (static graph-only, no LLM):

```bash
cargo run -p cih-engine -- wiki "$REPO"
```

Custom output directory:

```bash
cargo run -p cih-engine -- wiki "$REPO" --out /tmp/my-wiki
```

With LLM enrichment:

```bash
# DeepSeek
DEEPSEEK_API_KEY="sk-..." \
cargo run -p cih-engine -- wiki "$REPO" \
  --llm --llm-provider deepseek --llm-model deepseek-chat --llm-max-tokens 4096

# Google Gemini
GEMINI_API_KEY="AQ...." \
cargo run -p cih-engine -- wiki "$REPO" \
  --llm --llm-provider gemini --llm-model gemini-2.5-flash --llm-max-tokens 4096

# Anthropic Claude
ANTHROPIC_API_KEY="sk-ant-..." \
cargo run -p cih-engine -- wiki "$REPO" \
  --llm --llm-provider anthropic --llm-model claude-haiku-4-5-20251001

# OpenAI
OPENAI_API_KEY="sk-..." \
cargo run -p cih-engine -- wiki "$REPO" \
  --llm --llm-provider openai-compatible --llm-model gpt-4o-mini

# Local Ollama (no key needed)
cargo run -p cih-engine -- wiki "$REPO" \
  --llm --llm-provider openai-compatible \
  --llm-base-url http://localhost:11434/v1 --llm-model llama3:8b
```

Wiki modes (pass `--wiki-mode`):

| Mode | Behaviour |
| --- | --- |
| `graph` (default) | No LLM; renders from graph data only |
| `llm-summary` | Adds a short LLM summary to each page |
| `llm-full` | Full LLM-enriched content for every page |

Generate an HTML viewer alongside the Markdown:

```bash
cargo run -p cih-engine -- wiki "$REPO" --html
```

Process only specific communities (useful during development):

```bash
cargo run -p cih-engine -- wiki "$REPO" \
  --filter-community payment \
  --llm --llm-provider deepseek
```

Main outputs:

```text
$REPO/.cih/wiki/pages/
  index.md
  routes.md
  <feature>/
    index.md
    po.md
    ba.md
    dev/<class>.md
```

### Interactive TUI (`ui`)

`ui` opens an interactive terminal interface for building and running cih commands without
needing to remember flag names.

```bash
cargo run -p cih-engine -- ui
```

Navigation:
- **Arrow keys / j/k** — move between commands (left panel) and fields (right panel)
- **Enter** — select a command or toggle a field
- **Tab** — switch between the command list and the field panel
- **i** — enter edit mode for text fields
- **Esc** — exit edit mode / cancel
- **r** — review the assembled command and confirm to run it
- **q** — quit

The TUI covers: `scan`, `analyze`, `discover`, `embed`, and `wiki`. Set your env vars
(`FALKOR_URL`, `CIH_GRAPH_KEY`, etc.) before launching — the TUI inherits them.

### Interactive Wizard (`start`)

`start` is a guided step-by-step wizard that walks through env setup, FalkorDB and Postgres
startup, and the full indexing pipeline. Useful for first-time setup on a new machine.

```bash
cargo run -p cih-engine -- start
```

Non-interactive mode (scripting):

```bash
cargo run -p cih-engine -- start \
  --repo /path/to/java-project \
  --repo-name my-service \
  --postgres-password changeme \
  --non-interactive
```

Dry run (print the plan without writing files):

```bash
cargo run -p cih-engine -- start --repo /path/to/java-project --dry-run
```

### Repo Registry (`list`, `status`)

List all repos registered in `~/.cih/registry.json`:

```bash
cargo run -p cih-engine -- list
cargo run -p cih-engine -- list --json
```

Show registry status for a specific repo:

```bash
cargo run -p cih-engine -- status my-service
cargo run -p cih-engine -- status /absolute/path/to/java-project --json
```

### Group (cross-service contracts)

Manage multi-repo groups for cross-service HTTP and event contract analysis:

```bash
# Create a group
cargo run -p cih-engine -- group create my-group

# Add repos to the group
cargo run -p cih-engine -- group add my-group payment-service
cargo run -p cih-engine -- group add my-group order-service

# List groups
cargo run -p cih-engine -- group list

# Sync contract matches across the group
cargo run -p cih-engine -- group sync my-group

# Remove a repo from a group
cargo run -p cih-engine -- group remove my-group payment-service
```

### Features

Inspect and override feature grouping assignments detected during `discover`:

```bash
cargo run -p cih-engine -- features show "$REPO"
cargo run -p cih-engine -- features override "$REPO" --community payments --feature Payments
```

### Artifact (bundle import/export)

Export and import `.cih/` state for sharing or offline bootstrap:

```bash
# Export current artifacts to a bundle
cargo run -p cih-engine -- artifact export "$REPO"
cargo run -p cih-engine -- artifact export "$REPO" --out /tmp/my-bundle.zst

# Import a bundle (restores incremental state)
cargo run -p cih-engine -- artifact import "$REPO" --bundle /tmp/my-bundle.zst

# Bootstrap: import bundle + bulk-load into FalkorDB + register repo
cargo run -p cih-engine -- artifact bootstrap "$REPO" --bundle /tmp/my-bundle.zst
```

## Scope Selection

`analyze` requires at least one selector:

- `--all`
- `--module`
- `--include`
- A scope file

If you run `analyze "$REPO"` without a selector, the engine prints scan recommendations and exits without parsing.

### Scope File

By default, `analyze` looks for:

```text
$REPO/cih.scope.toml
```

Example:

```toml
all = false
modules = ["app", "infra"]
include = ["app/src/main/java/**/*.java"]
exclude = ["**/*Test.java", "**/generated/**"]
include_decompiled = false
```

You can also pass a custom scope file:

```bash
cargo run -p cih-engine -- analyze "$REPO" --scope /absolute/path/to/cih.scope.toml
```

Scope behavior:

- `all = true` selects all eligible Java files.
- `modules` selects files from named detected modules.
- `include` adds glob-matched Java files.
- `exclude` removes glob-matched files.
- `include_decompiled = true` allows `.workspace-dependencies/` files.

## Incremental Re-Indexing

The analyzer stores file hashes and parse-cache entries under `.cih/`. On later runs, unchanged files can reuse cached parse output.

Normal incremental run:

```bash
cargo run -p cih-engine -- analyze "$REPO" --all
```

Force reparsing:

```bash
cargo run -p cih-engine -- analyze "$REPO" --all --no-cache
```

Use `--no-cache` after parser changes, graph schema changes, or when you suspect stale parse output.

## MCP Server

Start the server:

```bash
export FALKOR_URL=redis://127.0.0.1:6380
export CIH_GRAPH_KEY=cih
export CIH_ARTIFACTS_DIR="$REPO/.cih/artifacts"
cargo run -p cih-server
```

Optional semantic search:

```bash
export CIH_PG_URL=postgres://cih:changeme@localhost:5433/cih
```

Server environment variables:

| Variable | Default | Purpose |
| --- | --- | --- |
| `CIH_BIND` | `127.0.0.1:8080` | HTTP bind address |
| `CIH_GRAPH_BACKEND` | `falkor` | Graph backend |
| `FALKOR_URL` | `redis://127.0.0.1:6379` | FalkorDB connection URL |
| `CIH_GRAPH_KEY` | `cih` | Graph name/key |
| `CIH_ARTIFACTS_DIR` | unset | Artifacts root for BM25 query fallback |
| `CIH_PG_URL` | unset | PostgreSQL pgvector URL for semantic search |

Available MCP tools:

| Tool | Purpose |
| --- | --- |
| `context` | Return entity details and immediate graph context |
| `impact` | Traverse upstream, downstream, or both directions |
| `communities` | Return discovered communities |
| `query` | Search graph/artifact text |
| `route_map` | Return HTTP route mappings |

Example tool calls:

```json
{"name": "OrderService"}
```

```json
{"name": "OrderService", "direction": "both", "max_depth": 2}
```

```json
{"limit": 10}
```

```json
{"q": "create order controller", "limit": 5, "expand": true}
```

```json
{"prefix": "/api", "limit": 50}
```

## Generated Files

When you analyze another repository, `yummy-cih` writes generated files inside that repository:

```text
.cih/
  repo-map.json
  scope.json
  file-hashes.json
  parse-cache/
  parsed/
  artifacts/
  artifacts-community/
```

These files are usually local analysis artifacts. In most application repositories, add `.cih/` to that repository's `.gitignore` unless you intentionally want to commit generated intelligence artifacts.

## Common Command Cheat Sheet

```bash
# From the yummy-cih repo root
export REPO=/absolute/path/to/java-repository

# Start backing services (FalkorDB on :6380, Postgres on :5433)
POSTGRES_PASSWORD=changeme docker compose up -d falkordb postgres

export FALKOR_URL=redis://127.0.0.1:6380
export CIH_GRAPH_KEY=cih
export CIH_PG_URL=postgres://cih:changeme@localhost:5433/cih

# Scan
cargo run -p cih-engine -- scan "$REPO"

# Analyze all code and load graph
cargo run -p cih-engine -- analyze "$REPO" --all

# Analyze a module only
cargo run -p cih-engine -- analyze "$REPO" --module app

# Analyze without loading graph
cargo run -p cih-engine -- analyze "$REPO" --all --no-load

# Re-run resolver only
cargo run -p cih-engine -- resolve "$REPO"

# Discover communities and process traces
cargo run -p cih-engine -- discover "$REPO"

# Embed nodes into pgvector
cargo run -p cih-engine -- embed "$REPO"

# Generate wiki docs
cargo run -p cih-engine -- wiki "$REPO"

# Open interactive TUI
cargo run -p cih-engine -- ui

# Start MCP server
export CIH_ARTIFACTS_DIR="$REPO/.cih/artifacts"
cargo run -p cih-server

# Run all tests
cargo test --workspace
```

## Troubleshooting

### The server cannot find my graph

Make sure the engine and server use the same values:

```bash
export FALKOR_URL=redis://127.0.0.1:6380
export CIH_GRAPH_KEY=cih
```

Then rerun:

```bash
cargo run -p cih-engine -- analyze "$REPO" --all
cargo run -p cih-server
```

### Port 6379 is already in use

Map FalkorDB to `6380` and always set `FALKOR_URL`:

```bash
docker run -d --name falkordb -p 6380:6379 falkordb/falkordb:latest
export FALKOR_URL=redis://127.0.0.1:6380
```

### `analyze` exits without parsing

You probably did not provide a scope selector. Use one of:

```bash
cargo run -p cih-engine -- analyze "$REPO" --all
cargo run -p cih-engine -- analyze "$REPO" --module app
cargo run -p cih-engine -- analyze "$REPO" --include "src/main/java/**/*.java"
```

### Query results are empty

Check that:

- `analyze` completed successfully.
- `discover` completed if you are asking about communities or traces.
- `CIH_ARTIFACTS_DIR` points at the analyzed repository's `.cih/artifacts`.
- `CIH_PG_URL` is set if you expect semantic vector search.

### Incremental analysis looks stale

Force a fresh parse:

```bash
cargo run -p cih-engine -- analyze "$REPO" --all --no-cache
```

### Decompiled files are missing

Files under `.workspace-dependencies/` are deferred by default. Include them explicitly:

```bash
cargo run -p cih-engine -- analyze "$REPO" --all --include-decompiled
```

### Embedding fails

Confirm `CIH_PG_URL` is set and the database has pgvector support enabled. Then rerun:

```bash
export CIH_PG_URL=postgres://cih:changeme@localhost:5433/cih
cargo run -p cih-engine -- embed "$REPO"
```
