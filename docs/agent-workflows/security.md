# CIH Workflow: Security Review

**Persona:** Developer, Security Reviewer, Tech Lead
**Goal:** Find source→sink taint paths — places where user-controlled data entering through
an HTTP handler or event listener can reach a dangerous operation (SQL execution, OS command,
file write, unencoded HTML output) — and turn them into review findings.

---

## When to use this workflow

- "Are there any SQL injection risks in this service?"
- "Does any user input reach Runtime.exec or ProcessBuilder?"
- "Security-review this PR / this module before release"
- "Which endpoints write user data to the filesystem?"

---

## Sink categories

| Category | Meaning | Severity |
|---|---|---|
| `sql` | Dynamic SQL string passed to a DB execution API (`Statement#execute*`, `JdbcTemplate#*`). Parameterized `PreparedStatement` use is treated as a sanitizer, not a sink. | high |
| `exec` | OS process execution (`Runtime#exec`, `ProcessBuilder`) | high |
| `file` | File-system write with caller-controlled path or content (`Files#write`, `FileWriter`, …) | medium |
| `html` | HTML/JS output without encoding — potential XSS (`PrintWriter#print`, `HttpServletResponse#getWriter`) | medium |

---

## Step-by-step

### Step 1 — Fast scan (Phase 0)

```
taint_paths({ category: "all" })
```

Runs an inter-procedural BFS on the call graph: every method with a `HANDLES_ROUTE` or
`LISTENS_TO` edge is a source; any method calling a known-dangerous API is a sink.
Fast (sub-second on most repos) but flow-insensitive — treat results as candidates.

Returns:
```json
{
  "total_found": 4,
  "returned": 4,
  "refined": false,
  "paths": [
    {
      "source": "Method:com.acme.OrderController#search/1",
      "sink_method": "Method:com.acme.OrderDao#findByRawSql/1",
      "category": "sql",
      "severity": "high",
      "confidence": 0.65,
      "hop_count": 2,
      "hops": ["Method:com.acme.OrderController#search/1", "Method:com.acme.OrderService#search/1", "Method:com.acme.OrderDao#findByRawSql/1"],
      "file": "src/main/java/com/acme/OrderController.java",
      "line": 42
    }
  ],
  "stats": { "phase0_paths": 4 }
}
```

### Step 2 — Confirm the interesting paths (refine)

```
taint_paths({ category: "sql", refine: true, min_confidence: 0.0 })
```

`refine: true` runs the flow-sensitive phases (intra-procedural liveness, CFG construction,
PDG kill-aware taint) on the source methods of candidate paths and adjusts `confidence` up
or down. Slower — it reads source files — but only for methods on candidate paths.
Paths whose confidence *rises* after refinement are the strongest findings; paths that drop
sharply were likely sanitized or never actually carried the tainted value.

### Step 3 — Read the evidence

For each path worth reporting, pull the actual code:

```
read_file({ path: "src/main/java/com/acme/OrderDao.java", start_line: 30, end_line: 60 })
```

and the surrounding call context:

```
context({ name: "Method:com.acme.OrderDao#findByRawSql/1" })
```

### Step 4 — Scope the fix

```
impact({ name: "Method:com.acme.OrderDao#findByRawSql/1", direction: "upstream" })
test_coverage({ name: "Method:com.acme.OrderDao#findByRawSql/1" })
```

`impact` shows every route that reaches the sink (all are affected until the sink is fixed);
`test_coverage` shows whether a regression test exists to pin the fixed behavior.

---

## Interpreting confidence

- Phase 0 scores favor **short paths** — a controller calling a raw-SQL DAO directly scores
  higher than a 6-hop chain.
- With `refine: true`, the flow-sensitive phases multiply the score: confirmed
  data flow raises it, detected sanitization or killed definitions lower it.
- `min_confidence` defaults to 0.5. For an exhaustive audit pass `min_confidence: 0.0`
  and triage everything; for a PR gate keep the default and treat any hit as blocking.

## Custom rules

Project-specific sinks/sanitizers go in `cih.taint.toml` at the repo root:

```toml
[[sink]]
pattern = "LegacyDao#runQuery"
category = "sql"

[[sanitizer]]
pattern = "SqlSafe#escape"
```

Rules merge with the built-ins (set `[settings] extend_defaults = false` to replace them).

## Output shape to return to the user

Group findings by category, ordered by confidence. For each: severity, the route/entry point,
the sink method with `file:line`, the hop chain, and the code snippet from Step 3. End with
the fix scope from Step 4 (affected routes, existing test coverage).

## Tips

- Always run Step 1 on the whole repo first — per-category calls hide cross-cutting sinks.
- A path ending in a `JdbcTemplate#query` sink whose method also calls
  `PreparedStatement#set*` will be down-scored on refine; verify by reading the code, not
  by trusting the score alone.
- The analysis is call-graph based: reflection, dynamic dispatch through frameworks, and
  SQL built outside string constants can be missed. Absence of findings is not proof of
  absence — say so in review summaries.
