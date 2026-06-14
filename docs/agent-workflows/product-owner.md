# CIH Workflow: Product Owner / Business Analyst

**Persona:** Product Owner, Business Analyst
**Goal:** Understand what the system does from a business perspective — which APIs exist, which
business processes are modelled, and how the codebase is organised by module — without reading
code.

---

## When to use this workflow

- "What features does this service expose?"
- "Show me the order lifecycle / payment flow"
- "What modules does the backend have and how big are they?"
- "Is the `/api/checkout` endpoint safe to change for the upcoming sprint?"
- "Map the current API surface for the tech-debt review"

---

## Step-by-step

### Step 1 — List indexed repositories

```
list_repos()
```

Returns: `[{ name, path, indexed_at, stats: { nodes, edges, files, routes, communities, processes } }]`

Confirm the service name and when it was last indexed. A stale index (indexed weeks ago) means
recent features may not appear. Check `stats.routes` to confirm this is an HTTP service.

### Step 2 — Browse the API surface

```
route_map({ limit: 200 })
```

Returns: `[{ path, http_method, decorator, handler_name, handler_qualified }]`

This is the complete HTTP API catalogue. Group by path prefix to identify feature areas:
- `/api/orders/**` — order management
- `/api/payments/**` — payment processing
- `/api/users/**` — user management

Filter to a specific area:

```
route_map({ prefix: "/api/orders", limit: 50 })
```

### Step 3 — Understand the module breakdown

```
communities({ limit: 15 })
```

Returns: `[{ id, name, symbol_count, cohesion }]`

Each community is a detected module cluster. `name` is typically the shared package prefix
(e.g. `com.acme.payment`). `symbol_count` indicates module size. `cohesion` (0–1) measures
internal coupling — high cohesion (>0.6) means a well-bounded module.

Read the full community list as a resource for richer detail:

```
cih://repo/{name}/communities
```

### Step 4 — Read named business processes

```
cih://repo/{name}/processes
```

Returns all Process-trace nodes — named execution flows extracted from the codebase
(e.g., "CreateOrder", "RefundPayment", "UserRegistration"). Each node has:
- `name` — human-readable process name
- `processType` — e.g. "transaction", "query", "event"
- `handler` — the entry-point method id

This is the closest CIH gets to a business-process diagram without a BPMN tool.

### Step 5 — Understand a specific feature / endpoint

Pick a route from Step 2 and look up the handler:

```
context({ name: "Method:com.acme.order.OrderController#submitOrder" })
```

Returns:
```json
{
  "node": { "kind": "Method", "name": "submitOrder", "file": "OrderController.java" },
  "callers": [...],
  "callees": [{ "name": "OrderService#completeOrder", ... }, ...],
  "processes": ["Process:CreateOrder"]
}
```

`callees` shows what the handler delegates to — the domain logic behind the endpoint.
`processes` links it to the named business flow.

### Step 6 — Check impact before requesting a change

If a sprint item involves changing an existing endpoint, scope the risk first:

```
detect_changes({ scope: "base_ref", base_ref: "feature/checkout-v2" })
```

Returns `{ changed_files, changed_symbols, affected_symbols, affected_processes, risk }`.

Present `risk` and `affected_processes` to the development team as the change-scope summary.

---

## Reading the module map for a tech-debt review

1. `communities({ limit: 20 })` — rank modules by `symbol_count` and `cohesion`
2. Low-cohesion modules (cohesion < 0.3) are poorly bounded — flag for refactoring
3. Very large modules (symbol_count > 500) are potential split candidates
4. `route_map()` — count routes per module prefix to identify over-loaded controllers

---

## Output shape to return to the user

```json
{
  "service": "<repo name>, indexed <date>",
  "api_surface": {
    "total_routes": 42,
    "areas": [
      { "prefix": "/api/orders", "route_count": 12 },
      { "prefix": "/api/payments", "route_count": 8 }
    ]
  },
  "modules": [
    { "name": "com.acme.payment", "symbols": 240, "cohesion": 0.71 }
  ],
  "business_processes": ["CreateOrder", "RefundPayment", "UserRegistration"],
  "summary": "<2–3 sentences for a non-technical audience>"
}
```

---

## Tips

- `route_map` is always the right first tool for a PO conversation — it speaks HTTP,
  not Java.
- `communities` + `processes` give a business-readable view without requiring code knowledge.
- Never share raw `node_id` strings with non-technical stakeholders — translate to
  `http_method + path` or `process name`.
- If `stats.processes == 0`, the discover step has not been run; advise the team to run
  `cih-engine discover <repo>` so process traces are available.
- Use `detect_changes` before sprint planning when the ticket touches a shared service —
  it surfaces blast radius before estimation.
