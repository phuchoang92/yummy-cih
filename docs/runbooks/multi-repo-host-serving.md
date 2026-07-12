# Serving a multi-repo group over MCP (one host server)

`cih-server` is **multi-repo**: one instance fronts a whole group. A tool's
optional `repo` arg selects the target service; empty = the server's *primary*
(`CIH_GRAPH_KEY` / `CIH_ARTIFACTS_DIR`). The server connects to each member's
graph on demand (all member graphs live in one FalkorDB, keyed per repo) and
resolves each member's `artifacts_dir` from the `Registry`
(`$HOME/.cih/registry.json`). Set `CIH_GROUP` so `list_repos` scopes to the
group's members.

- **Deep tools** (`context`, `impact`, `route_map`, `communities`, `trace_flow`,
  `search_code`, `query`, `feature_map`, `complexity_hotspots`,
  `find_duplicates`, `test_coverage`, `regression_scope`, `untested_paths`,
  `detect_changes`, `taint`, `read_file`, `grep_files`, wiki) — take `repo`;
  omit it for the primary.
- **Cross-repo group tools** (`list_repos`, `status`, `group_contracts`,
  `api_impact`, `trace_flow_x`, `shape_check`) — artifacts-based; always span
  the whole group.

So the whole microservice is **one endpoint**. A team registers a single MCP
server and passes `repo=` when they want a specific service.

Run it on the **host** (not the compose `cih-server` container): the host
registry's `artifacts_dir`/`path` values are host paths, so the server process
must see those same paths. The container has a different filesystem view
(`/repo`, its own `cih-home` volume), which is why the Dockerized server can't
serve a host-indexed group without re-indexing in-container.

> Older single-graph builds could only serve one repo per process; the fleet
> pattern (one server per repo, each on its own port, sharing `$HOME/.cih`) is
> the fallback if you ever pin to such a build — but the multi-repo server
> supersedes it.

## Prerequisites

1. Datastores up. The compose `falkordb` (host **:6380**) and `postgres`
   (**:5433**) services are enough — you do **not** need the compose
   `cih-server`. If it's running against a now-stale graph key, stop just it:
   `docker stop <stack>-cih-server-1` (leave `falkordb`/`postgres` running).
2. Each repo analyzed + discovered on the host into its **own** graph key
   (never share the default `cih` key across repos):
   ```
   cih-engine analyze  <repo> --all --graph-key <key> --no-cache
   cih-engine discover <repo> --graph-key <key>
   ```
3. The group created and synced:
   ```
   cih-engine group create <group>
   cih-engine group add <group> <repo-name>   # once per member
   cih-engine group sync <group>               # writes ~/.cih/groups/<group>/contracts.jsonl
   ```
4. `cargo build --release -p cih-server`.

## Server configuration

All env-driven (`crates/cih-server/src/config.rs`):

| Env | Value |
|-----|-------|
| `CIH_BIND` | `127.0.0.1:<port>` — loopback ⇒ no auth token required |
| `CIH_GRAPH_KEY` | the **primary** repo's graph key (deep tools use it when `repo` is empty) |
| `CIH_GROUP` | the home group name — scopes `list_repos` to its members |
| `CIH_ARTIFACTS_DIR` | the **primary** repo's **unversioned** `<repo>/.cih/artifacts` parent (BM25 resolves the latest versioned subdir) |
| `FALKOR_URL` | `redis://127.0.0.1:6380` |
| `CIH_PG_URL` | `postgres://<user>:<pass>@127.0.0.1:5433/<db>` — optional; enables semantic `query`. BM25 `search_code` works without it |

Member repos other than the primary need no extra config: the server resolves
each member's graph key + `artifacts_dir` from the `Registry` when a tool's
`repo` arg names them, connecting/caching a `FalkorStore` per key on demand.

> Follow-up: semantic `query` shares one pgvector connection across repos; if
> embeddings aren't graph/repo-scoped, semantic hits may span repos. BM25
> `search_code` is correctly per-repo.

## Worked example — the 212ecom (dienmaychiben) stack

Three repos, three languages, group `dienmaychiben` (140 be→fe HTTP-route
contracts), served by **one** server. Launcher: `scripts/serve-212ecom.sh`
(`start` | `stop` | `status`).

| Endpoint | Port | Primary key | Group | Serves |
|----------|------|-------------|-------|--------|
| cih-212ecom | 8080 | `212ecom_be` (Java) | `dienmaychiben` | be (default) + fe/ai via `repo=` + cross-repo |

```
scripts/serve-212ecom.sh start     # frees :8080, launches, prints health + examples
scripts/serve-212ecom.sh status
scripts/serve-212ecom.sh stop
```

Log: `/tmp/cih-212ecom/cih-212ecom.log`; PID at `cih-212ecom.pid`.

## Registering the endpoint in an MCP client

One endpoint for the whole stack:

```
claude mcp add --transport http cih http://localhost:8080/mcp
```

Then pass `repo` to target a service:
- `route_map {}` → be (Java, primary); `route_map {"repo":"212ecom-ai"}` → Python FastAPI.
- `context {"name":"...","repo":"212ecom-fe"}` → TypeScript.
- `list_repos`, `api_impact`, `trace_flow_x`, `shape_check` → span all three.

## Smoke test (no MCP client needed)

`GET /health` and `/ready` return 200. For tool calls, speak MCP Streamable HTTP
directly (initialize → `notifications/initialized` → `tools/call`; each POST is
one JSON-RPC message, `--data-raw`, `Accept: application/json,
text/event-stream`, echo the `Mcp-Session-Id` response header on follow-ups).

Representative checks (all against the one endpoint):
- `list_repos` → the group's members (scoped by `CIH_GROUP`).
- `route_map {"repo":"212ecom-ai"}` → the Python FastAPI routes; `route_map {}` → be Spring routes.
- `context {"name":"AdminBrandService","repo":"212ecom-be"}` → resolves on be.
- `api_impact {group, method, path}` for a provider route → lists consumer repos.
- `trace_flow_x {repo:"212ecom-fe", entry_point:"ExternalEndpoint:DELETE:/api/v1/admin/brands/{*}", group:"dienmaychiben"}`
  → a `CONTRACT` hop into `212ecom-be`'s route, then `HANDLES_ROUTE` →
  controller → service (`CALLS`), proving the cross-language trace.
