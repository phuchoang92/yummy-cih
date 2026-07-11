# CIH — Code Intelligence for this repo

This project (yummy-cih) is itself a Code Intelligence Hub: a Rust MCP server that
indexes a codebase into a graph and answers structural questions over it.

There are two ways an agent works here — read the section that matches your task:

- **Using CIH** (a CIH MCP server is connected, pointed at some target codebase):
  prefer its tools over grep/read for structure, impact, and navigation. See
  *Always/Never Do*, *Tools*, *Resources* below.
- **Developing yummy-cih** (editing this Rust repo): see *Developing CIH itself* at
  the bottom for build/lint/test conventions and repo layout. The *Using CIH* rules
  ("run impact before editing") only apply when a CIH instance has actually indexed
  this repo — don't assume one is running.

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
| Cross-repo contracts | `group_contracts`, `api_impact` (`include_callers` walks consumer call chains), `shape_check` |
| Cross-repo execution chain | `trace_flow_x` (hops repos via group contracts; steps carry `repo` + `via.kind == "CONTRACT"` at crossings) |
| Read source (size-capped) | `read_file` |
| Registry / freshness | `list_repos`, `status` |

Contract freshness: groups re-sync automatically after `analyze`/`discover`/`refresh`
of a member repo (disable with `CIH_NO_AUTO_GROUP_SYNC=1`). `status` reports per-group
`contracts_synced_at`/`stale`, and every contract tool response carries
`contracts_synced_at`/`contracts_stale` — treat stale contracts as suspect and re-run
`cih-engine group sync <group>` (or check `cih-engine group status <group>`).
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

**Layout.** ~16 crates under `crates/`; two binaries, both thin shims over their
library crates: `cih-engine` (CLI — the `scan → parse → resolve → load → discover
→ embed → wiki` pipeline, writes `.cih/` artifacts; clap surface + all command
implementations live in `crates/cih-engine/src/cmd/`, per-command settings
resolution in `settings.rs`) and `cih-server` (the MCP server, streamable HTTP via
rmcp 0.7; tools in `crates/cih-server/src/app.rs`). The graph store trait is
`cih-graph-store` with the `cih-falkor` adapter.

**Build/test.** `cargo build`, `cargo test --workspace`. Workspace tests are hermetic
— no FalkorDB/Postgres needed (integration paths use artifact fixtures). Local services
when you do need them: FalkorDB on **6380** (Homebrew redis squats 6379), Postgres on
5433 → `FALKOR_URL=redis://127.0.0.1:6380`.

**Lint gate** (`.github/workflows/ci.yml`). Blocking: `cargo clippy --workspace
--all-targets -- -D warnings` plus `cargo test --workspace` — keep the whole tree
warning-clean, and `cargo fmt --all --check` (the tree is fmt-normalized).

**Parse-cache schema.** Bump `cih_lang::PARSE_CACHE_SCHEMA` whenever any
parser/extractor changes the shape or content of `ParsedUnit` output — the
per-file parse cache (`.cih/parse-cache/v<N>/`) and the analyze no-op gate both
key off it; without a bump, unchanged files silently keep stale cached output
after an upgrade. The `parse_schema_guard` test (cih-engine) fails on any
parser-output drift until the schema and its paired `GOLDEN` hash are updated
together.
Note: `browser.rs`/`layout.rs` in cih-server are the live graph-browser UI served at
`/graph` (tested by `tests/browser.rs`) — not dead code. Both binaries are thin
shims: server logic lives in `cih_server` (`src/app.rs`), engine modules in
`cih_engine` (used by `main.rs` via `use cih_engine::…`).

**Config files** (per-repo, at the target repo root): `cih.toml` (analyze/discover/wiki
option defaults — layered flag > env > repo `cih.toml` > `~/.cih/config.toml` > default;
`cih config init`/`show` manage it), `cih.scope.toml` (analyze scope), `cih.taint.toml`
(taint rules), `cih.decompile.toml` (decompile). `.env` holds infra + LLM keys. Adding a
new persisted option means making its clap flag `Option<T>` (in
`crates/cih-engine/src/cmd/args.rs`), adding it to the settings schema and the
matching `resolve_*` function in `crates/cih-engine/src/settings.rs`.

**Conventions.**
- Write implementation plans as markdown in `docs/plans/`; parser assumptions/known
  graph limits are documented in `docs/ARCHITECTURE.md`.
- Security posture (mandatory auth on non-loopback bind, `ask_codebase` LLM egress):
  see `SECURITY.md`. Keep `ask_codebase` off for sensitive codebases.
- Don't commit on the default branch — branch first; commits/PRs only when asked.
