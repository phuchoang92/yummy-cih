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

### Step 2 — Read the repo context resource

```
cih://repo/{name}/context
```

Returns the `RegistryEntry` JSON: node/edge counts, artifact paths, last-indexed git HEAD.
Use `stats.routes` to gauge if this is an HTTP service. Use `stats.communities` and
`stats.processes` to know whether community detection has been run.

### Step 3 — Browse high-level module clusters

```
communities({ limit: 10 })
```

Returns: `[{ id, name, symbol_count, cohesion }]` sorted by size descending.

Each community is a detected module cluster (Louvain). `name` is the common package prefix.
`cohesion` (0–1) measures how densely connected the cluster is. Start with the 3–5 largest.

### Step 4 — Find symbols by keyword

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

### Step 5 — Get 360° context for a key symbol

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

### Step 6 — List HTTP entry points (for service APIs)

```
route_map({ limit: 50 })
```

Returns: `[{ path, http_method, decorator, handler_id, handler_name, handler_qualified }]`

Filter by prefix to focus on a subsystem:

```
route_map({ prefix: "/api/orders", limit: 20 })
```

### Step 7 — Read process traces (optional, if discover has run)

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

- Start broad (`communities`, `route_map`) before going deep (`context`, `impact`).
- `query` with `expand: true` is the fastest way to surface unknown symbol names.
- If `stats.routes == 0` this is a library, not a service — skip `route_map`.
- If `stats.communities == 0`, discovery has not run; advise user to run
  `cih-engine discover <repo>`.
