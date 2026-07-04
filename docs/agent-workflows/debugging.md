# CIH Workflow: Debugging (Call-Chain Tracing)

**Persona:** Developer
**Goal:** Trace the execution path of a bug — find where a call originates, what it touches, and
which process it belongs to — without running the application.

---

## When to use this workflow

- "Where is this exception thrown from?" — trace callers
- "Why is this method being called with bad data?" — trace the call chain
- "Which code path leads from the HTTP endpoint to this repository method?"
- "What does this service do when it receives a payment event?"

---

## Step-by-step

### Step 1 — Locate the symbol where the bug manifests

Start with a keyword or class/method name:

```
query({ q: "PaymentService processPayment" })
```

Returns: `{ hits: [{ node_id, name, kind, file, score }] }`

If you already know the full id (e.g. from a stack trace), skip to Step 2.

### Step 2 — Get full context for the suspect symbol

```
context({ name: "Method:com.acme.PaymentService#processPayment" })
```

Returns:
```json
{
  "node": { "id": "...", "kind": "Method", "name": "processPayment", "file": "...", "range": {...} },
  "callers": [{ "id": "Method:...", "name": "...", "file": "..." }, ...],
  "callees": [{ "id": "Method:...", "name": "...", "file": "..." }, ...],
  "processes": ["Process:CreateOrder"]
}
```

- `callers` — all methods that call this one (direct, 1 hop)
- `callees` — all methods this one calls (direct, 1 hop)
- `processes` — named execution flows this symbol is part of

### Step 3 — Trace upstream: who triggers this code path?

```
impact({ name: "Method:com.acme.PaymentService#processPayment", direction: "upstream", max_depth: 6 })
```

Returns `affected` sorted by BFS depth. Depth-1 entries are direct callers; follow the depth
ladder upward to find the entry point (HTTP handler, event listener, scheduler).

Look for `Route` kind nodes in the affected list — they are HTTP entry points:
```json
{ "id": "Route:POST:/api/payments", "depth": 3, "via": "CALLS" }
```

### Step 4 — Find the HTTP entry point

Once you have a Route node or handler method from Step 3:

```
route_map({ prefix: "/api/payments" })
```

Returns: `[{ path, http_method, handler_id, handler_name }]`

This confirms which HTTP method and path leads into the buggy code path.

### Step 5 — Trace downstream: what does this call touch?

```
impact({ name: "Method:com.acme.PaymentService#processPayment", direction: "downstream", max_depth: 4 })
```

Downstream impact shows what repositories, external clients, and utilities this method
reaches. Useful for understanding side effects (DB writes, event publishing, cache invalidation).

### Step 6 — Expand the local call neighbourhood

```
query({ q: "processPayment", expand: true })
```

With `expand: true`, returns a `subgraph` containing the matching nodes plus their
1-hop neighbours (all edge types, not just CALLS). Useful for seeing field accesses, imports,
and interface implementations alongside the call graph.

### Step 7 — Confirm process membership

From `context`, check `processes`. If the method belongs to a named process (e.g.
"Process:CreateOrder"), read the full process trace:

```
cih://repo/{name}/processes
```

Returns all process-trace nodes. Find the relevant one by `name` and inspect its
`handler` property to see the ordered list of steps.

---

## Reading a stack trace with CIH

Given a Java stack trace like:
```
com.acme.payment.PaymentService.processPayment(PaymentService.java:87)
com.acme.order.OrderService.completeOrder(OrderService.java:134)
com.acme.api.OrderController.submitOrder(OrderController.java:56)
```

1. `query({ q: "processPayment PaymentService" })` → find the NodeId for the bottom frame
2. `impact({ name: "<id>", direction: "upstream", max_depth: 3 })` → verify the call chain
   matches the stack trace (confirms the graph is consistent with the actual runtime path)
3. `context({ name: "<id>" })` → understand what `processPayment` calls (callees)
4. If a callee looks like the bug source, repeat from that node

---

## Output shape to return to the user

```json
{
  "entry_point": "POST /api/payments → OrderController#submitOrder",
  "call_chain": [
    "OrderController#submitOrder (depth 3)",
    "OrderService#completeOrder (depth 2)",
    "PaymentService#processPayment (depth 1)"
  ],
  "process": "Process:CreateOrder",
  "callees_of_suspect": ["PaymentRepository#save", "EventPublisher#publish"],
  "hypothesis": "<one sentence: where the bug likely originates>"
}
```

---

## Tips

- Use `max_depth: 6` for deeply nested call chains; the default (4) may miss entry points in
  layered architectures (Controller → Service → UseCase → Domain → Repository).
- `callers` in `context` shows only direct (1-hop) callers; use `impact` for transitive chains.
- Route nodes in the impact list (kind `Route`) are always entry points — surface them first.
- If the suspect symbol is an interface method, look for `MethodImplements` edges in the
  subgraph — the concrete impl may be in a different class.
