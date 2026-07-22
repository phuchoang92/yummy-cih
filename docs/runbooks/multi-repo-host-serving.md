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
- **Admin tools** (`index_repo`, `index_status`, `index_cancel`,
  `add_resolve_pattern`, `list_resolve_patterns`) — these **spawn `cih-engine`
  on the host**; see "Indexing from the server" below and `docs/SECURITY.md` §3.

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

### Memory budgets (validated at startup)

Resident caches are bounded per family, and **the server refuses to start when the
families sum above the total** — the error names `CIH_CACHE_MAX_BYTES`, so it is
worth setting these deliberately on a multi-repo host.

| Env | Default | Bounds |
|-----|---------|--------|
| `CIH_CACHE_MAX_BYTES` | 1040 MiB | total; must be ≥ the sum of the four below |
| `CIH_ARTIFACT_CACHE_MAX_BYTES` | 512 MiB | parsed `nodes.jsonl`/`edges.jsonl` snapshots |
| `CIH_WIKI_CACHE_MAX_BYTES` | 256 MiB | wiki indexes, resident renderers, live search |
| `CIH_SEARCH_CACHE_MAX_BYTES` | 256 MiB | per-repo BM25 indexes |
| `CIH_SEARCH_CACHE_MAX_ENTRIES` | 32 | repository/version index safety cap |
| `CIH_RESOURCE_INDEX_CACHE_MAX_BYTES` | 16 MiB | JSONL resource paging indexes |
| `CIH_ARTIFACT_CACHE_MAX_ENTRIES` | 32 | retained repo versions (LRU beyond this) |
| `CIH_ARTIFACT_CACHE_IDLE_TTL_SECS` | 1800 | idle eviction (0 disables) |

A single value larger than its budget is served **without being retained**, so an
oversize repository degrades to "no caching" rather than evicting healthy
entries or exceeding the budget. At ~500k nodes one repository's snapshot alone
exceeds the 512 MiB default (see `docs/perf/scale-500k.md`) — raise the artifact
and total budgets if you serve repositories that large and want them cached.

Other tuning knobs: `CIH_BLOCKING_MAX_CONCURRENT` (2) and
`CIH_BLOCKING_QUEUE_TIMEOUT_SECS` (5) bound concurrent cold artifact loads;
`CIH_BLOCKING_TIMEOUT_SECS` (90) is the per-load deadline;
`CIH_RESOURCE_MAX_BYTES` (256 KiB) caps one resource page;
`CIH_DETECT_CHANGES_MAX_SYMBOLS` (200) caps blast-radius traversals per
`detect_changes` call.

### Search and grep admission

Search indexes are generated as `search-index.bin` during analyze. On a cold
request the server validates source identity, schema fingerprint, checksum, and
format before admitting a decode under both a count limit and a transient-byte
budget. Missing, stale, or corrupt sidecars fall back to one streaming build and
an atomic repair; concurrent callers for the same version share that result.

| Env | Default | Purpose |
|-----|---------|---------|
| `CIH_SEARCH_SIDECAR_ENABLED` | `true` | rollback switch for sidecar loading/publication |
| `CIH_SEARCH_SCORE_MAX_CONCURRENT` | min(4, CPUs) | warm scorer lane |
| `CIH_SEARCH_SCORE_QUEUE_TIMEOUT_MS` | 2000 | scorer admission timeout |
| `CIH_SEARCH_COLD_MAX_CONCURRENT` | 1 | simultaneous decode/build count |
| `CIH_SEARCH_COLD_MAX_BYTES` | 512 MiB | aggregate cold transient reservation |
| `CIH_SEARCH_COLD_QUEUE_TIMEOUT_SECS` | 5 | cold count/byte admission timeout |
| `CIH_GREP_MAX_CONCURRENT_REQUESTS` | 1 | repository scans admitted at once |
| `CIH_GREP_THREADS` | min(4, CPUs) | process-wide dedicated grep workers |
| `CIH_GREP_QUEUE_TIMEOUT_SECS` | 2 | wait before `grep capacity saturated` |
| `CIH_GREP_DEADLINE_SECS` | 80 | cooperative partial-result deadline |
| `CIH_WIKI_LIVE_MAX_NODES` | 100000 | require generated wiki above this graph size |

Startup rejects zero/invalid values and rejects a grep queue plus scan deadline
that does not leave five seconds before `CIH_BLOCKING_TIMEOUT_SECS`.

Operational state is available at authenticated `GET /operations/metrics`.
Use it to distinguish slow execution from admission pressure: blocking
`queued`/`active` and cumulative queue wait describe cold read pressure, while
index `queued`/`running`/`rejected` describes analysis-job pressure. Request
completion logs use the `request_completed` event and include duration, queue
wait, response bytes, result count when available, completeness, and a bounded
error class.

The `retrieval` object adds search cache hits/misses/retained bytes/evictions,
scorer scratch and queue pressure, cold reserved bytes, sidecar
load/fallback/repair counters, wiki manifest/live-build counters, and grep
active/queued/rejected/partial/file totals. A growing `fallback_builds` count
with no `repair_succeeded` indicates stale artifacts or a read-only artifact
mount. A growing grep `rejected` count means callers should narrow their glob or
retry after the current scan drains.

The scheduled `.github/workflows/cih-server-soak.yml` workflow runs both the
ten-service and fifty-registry-entry matrices. For a local smoke run:

```bash
CIH_SOAK_DURATION_SECS=60 CIH_SOAK_REPOSITORIES=3 \
  CIH_SOAK_LARGE_NODES=50000 CIH_SOAK_SMALL_NODES=10000 \
  cargo run --release -p cih-server --example soak_bench
```

### Indexing from the server

`index_repo` spawns `cih-engine analyze` on the host, so it is bounded and
explicitly targeted:

- **Graph key.** A repo already in the registry re-indexes under **its own** key.
  A path not yet registered requires an explicit `graph_key`, and a key owned by
  a different repo is rejected — the server's primary key is never reused
  implicitly.
- **Admission.** `CIH_INDEX_MAX_CONCURRENT` (1) running, `CIH_INDEX_QUEUE_CAPACITY`
  (16) queued; one active job per repo (duplicates coalesce onto it); excess is
  rejected rather than queued without bound.
- **Deadline and output.** `CIH_INDEX_TIMEOUT_SECS` (1800) kills the child;
  `CIH_INDEX_OUTPUT_CAP_BYTES` (1 MiB) caps retained stdout/stderr per stream.
- **Cancellation.** `index_cancel(job_id=…)` kills a running child; poll
  `index_status` until it settles as `cancelled`.

There is **no path allow-list** — any directory readable by the server process can
be indexed. Keep `/mcp` authenticated (`docs/SECURITY.md` §1, §3).

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
directly (initialize → `notifications/initialized` → `tools/list` → `tools/call`;
each POST is one JSON-RPC message, `--data-raw`, `Accept: application/json,
text/event-stream`, echo the `Mcp-Session-Id` response header on follow-ups).

Representative checks (all against the one endpoint):
- `tools/list` → the full tool surface. **Required compatibility check:** it must
  be **non-empty** — a discovery-based client (Codex/Kiro/Claude) that sees `[]`
  here concludes CIH has no tools and silently does nothing.
- `list_repos` → the group's members (scoped by `CIH_GROUP`).
- `route_map {"repo":"212ecom-ai"}` → the Python FastAPI routes; `route_map {}` → be Spring routes.
- `context {"name":"AdminBrandService","repo":"212ecom-be"}` → resolves on be.
- `api_impact {group, method, path}` for a provider route → lists consumer repos.
- `trace_flow_x {repo:"212ecom-fe", entry_point:"ExternalEndpoint:DELETE:/api/v1/admin/brands/{*}", group:"dienmaychiben"}`
  → a `CONTRACT` hop into `212ecom-be`'s route, then `HANDLES_ROUTE` →
  controller → service (`CALLS`), proving the cross-language trace.
