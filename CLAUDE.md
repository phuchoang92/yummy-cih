# CIH — Code Intelligence for this repo

This project (yummy-cih) is itself a Code Intelligence Hub: a Rust MCP server that
indexes a codebase into a graph and answers structural questions over it. When an
agent has the **CIH MCP server** connected, prefer its tools over grep/read for
understanding structure, assessing change impact, and navigating safely.

> Connect (HTTP): `claude mcp add --transport http cih http://localhost:8080/mcp`
> Index/refresh a repo: `index_repo(repo_path="/abs/path")` → poll `index_status(job_id=...)`,
> or from the CLI `cih-engine analyze <repo>`.

## Always Do

- **Run impact analysis before editing a symbol.** `impact(name="OrderService", direction="upstream")`
  and report the blast radius (callers, affected processes, risk) before changing a
  function/class/method. Warn on HIGH/CRITICAL risk before proceeding.
- **Run `detect_changes` before committing** to confirm the change only touches the
  expected symbols. For a branch: `detect_changes(scope="base_ref", base_ref="main")`.
- **Explore by query, not grep**: `search_code(query="concept")` or `query(...)` to find
  relevant symbols; `context(name="Symbol")` for callers/callees/processes;
  `trace_flow(entry_point="Route:POST /path")` to follow a request end-to-end.
- **Security review**: `taint_paths(category="sql"|"exec"|"file"|"html")` for source→sink
  flows; `refine=true` for flow-sensitive confirmation. See `docs/agent-workflows/security.md`.

## Never Do

- NEVER edit a function/class/method without first running `impact` on it.
- NEVER ignore HIGH or CRITICAL risk from impact analysis.
- NEVER commit without running `detect_changes` to check the affected scope.

## NodeId format

Full: `Kind:fully.qualified.Name` (e.g. `Class:com.acme.OrderService`,
`Method:com.acme.OrderService#save/1`, `Route:POST /api/orders`). Short names
(e.g. `OrderService`) also work and trigger disambiguation — the tool returns
`{"status":"ambiguous","candidates":[...]}` when several match.

## Tools

| Task | Tool |
|------|------|
| Symbol context (callers/callees/processes) | `context` |
| Blast radius of a change | `impact` |
| End-to-end request/execution chain | `trace_flow` |
| All HTTP routes (OpenAPI export) | `route_map` |
| Keyword/semantic search | `search_code`, `query` |
| Business keyword → code clusters | `feature_map`, `communities` |
| Git-aware change impact | `detect_changes` |
| Tests to re-run / coverage gaps | `regression_scope`, `test_coverage`, `untested_paths` |
| Source→sink taint (SQLi, exec, file, XSS) | `taint_paths` |
| Complexity / duplication | `complexity_hotspots`, `find_duplicates` |
| Cross-repo contracts | `group_contracts`, `api_impact`, `shape_check` |
| Read source (size-capped) | `read_file` |
| Registry / freshness | `list_repos`, `status` |
| Index a repo | `index_repo`, `index_status` |

## Resources

| Resource | Use for |
|----------|---------|
| `cih://repo/{name}/context` | Registry entry, stats, index freshness |
| `cih://repo/{name}/communities` | Functional module clusters |
| `cih://repo/{name}/processes` | Named execution flows |
| `cih://repo/{name}/schema` | Graph node kinds + edge kinds |

## Workflow guides

Persona playbooks (when-to-use, step-by-step tool calls, output shape) live in
`docs/agent-workflows/`: `exploring.md`, `impact-analysis.md`, `debugging.md`,
`product-owner.md`, `tester.md`, `security.md`. Parser assumptions and known graph
limitations are in `docs/ARCHITECTURE.md`.

## Developing CIH itself

- Build/test: `cargo build`, `cargo test --workspace`. CI gates fmt (non-blocking
  today), clippy `-D warnings` on the backend crates, and the full test suite —
  see `.github/workflows/ci.yml`.
- Local services: FalkorDB on **6380** (Homebrew redis squats 6379), Postgres on 5433.
  `FALKOR_URL=redis://127.0.0.1:6380`.
- Command defaults: `analyze`/`discover`/`wiki` flags can be persisted in `<repo>/cih.toml`
  or `~/.cih/config.toml` (precedence: flag > env > repo > home > default). `cih config init`
  scaffolds it, `cih config show` prints effective values + source. See README "Configuration".
- Security posture (auth, LLM egress): see `SECURITY.md`.
