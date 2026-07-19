# CIH Workflow: Codebase Exploration

**Persona:** Any (Developer, PO, BA, Tester)
**Goal:** Orient to an unfamiliar codebase — understand its modules, entry points, and key symbols
without reading source files.

---

## When to use this workflow

- First session on a new repository
- Answering "what does this codebase do?" or "where is X implemented?"
- Mapping the system before planning a change

---

## Step-by-step

### Step 1 — Confirm what repos are indexed

```
list_repos()
```

Returns: `[{ name, path, indexed_at, stats: { nodes, edges, files, routes, communities } }]`

Pick the repo whose `name` or `path` matches the user's question. If multiple repos are indexed,
confirm which one is in scope before proceeding.

### Step 2 — Get the one-call orientation

```
architecture_overview(repo="<name>")
```

One compact, size-capped response with everything the old multi-call orientation
chained together:

- `stats` — per-kind node/edge counts (is this an HTTP service? how big?)
- `modules` — detected module clusters, each with `anchor_symbols`: canonical
  NodeIds ready to paste into `context(name=...)` / `impact(name=...)`
- `route_groups` — endpoints bucketed by path prefix, with samples like
  `Route:POST /api/v1/loans → Method:acme.LoanApi#create` (the left half is
  `trace_flow`-ready, the right half is `context`-ready)
- `entrypoints` — schedulers/event listeners (from discover) + high-degree hubs
- `wiki_pages` — slugs for `get_wiki_page(slug=...)`
- `provenance` + `warnings` — where each number came from and what is stale

Read the response like this:

- **`available: false` on a section is a pipeline fact, not a codebase fact** —
  its `reason`/`remedy` say which `cih-engine` step to run. Never report it as
  "this repo has no modules".
- Truncated lists carry a copy-pasteable `next` call — use that narrow tool
  instead of re-calling the overview with a bigger `limit`.
- Call the overview **once per repo per session**; everything after this step is
  drill-down.
- `hotspots` (complexity) is opt-in: `architecture_overview(sections=["hotspots"])`.
- If the server fronts a group, a `group` block lists members (exact `repo`
  strings for targeting) and contract freshness.

### Step 3 — Find symbols by keyword (when the overview didn't name them)

```
query({ q: "<keyword>" })
```

Returns: `{ hits: [{ node_id, name, kind, file, score }], subgraph? }`

Use natural-language terms ("payment", "authentication", "order") or Java class names.
BM25 ranks by term frequency in the symbol name, qualified name, and file path.

To expand a hit into its local neighbourhood:

```
query({ q: "<keyword>", expand: true })
```

Returns a `subgraph` of the top-5 hits plus their 1-hop neighbours (nodes + edges).

### Step 4 — Get 360° context for a key symbol

Seed this from a module's `anchor_symbols` or a route sample's handler id:

```
context({ name: "Class:com.acme.OrderService" })
```

Returns: `{ node, callers: [...], callees: [...], processes: [...] }`

- `callers` — who calls this symbol (upstream dependencies)
- `callees` — what this symbol calls (downstream)
- `processes` — named execution flows this symbol participates in

You can pass a short name like `"OrderService"` instead of a full NodeId. If multiple symbols
match you will receive `{ status: "ambiguous", candidates: [...] }` — pick the right one and
retry with the full id.

### Step 5 — Drill into one route subsystem

Take a `route_groups` prefix from the overview:

```
route_map({ prefix: "/api/orders", limit: 20 })
```

Returns: `[{ path, http_method, decorator, handler_id, handler_name, handler_qualified }]`

Then follow one end-to-end with a sample route id from the overview:

```
trace_flow({ entry_point: "Route:POST /api/orders" })
```

### Step 6 — Read process traces (optional, if discover has run)

```
cih://repo/{name}/processes
```

Returns JSON array of process-trace nodes — named execution flows (e.g., "CreateOrder",
"RefundPayment"). Each node has `name` and `processType`. Cross-reference with `context`
to see which symbols participate.

---

## Output shape to return to the user

```json
{
  "modules": ["<community name> — <symbol_count> symbols, cohesion <n>", ...],
  "key_symbols": ["<kind>:<fqn>", ...],
  "routes": ["<HTTP_METHOD> <path>", ...],
  "summary": "<2–3 sentence plain-English overview>"
}
```

---

## Tips

- `architecture_overview` first, narrow tools after — the overview exists so you
  don't chain `status` + `communities` + `route_map` + `search_wiki` by hand.
- Trust the `warnings` block over any single count: registry, graph, and wiki are
  differently-timestamped sources, and the overview reconciles them for you.
- `query` with `expand: true` is the fastest way to surface unknown symbol names.
- If `route_groups.total_routes == 0` this is a library, not a service.
- A section with `available: false` names its remedy (`cih-engine discover <repo>`,
  `cih-engine wiki <repo>`) — relay that command to the user verbatim.
