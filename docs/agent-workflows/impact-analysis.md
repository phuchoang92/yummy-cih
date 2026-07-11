# CIH Workflow: Impact Analysis (Blast Radius)

**Persona:** Developer, Tech Lead
**Goal:** Answer "what breaks if I change X?" before touching a symbol, or after staging a diff.

---

## When to use this workflow

- Before modifying a shared service or utility class
- After staging a diff, to score the risk of a PR
- When a PM asks "is it safe to change the payment flow?"

---

## Step-by-step

### Option A — Diff-driven (recommended for code review / PR prep)

Run this when you have staged changes or a branch to compare.

#### Step 1 — Score the staged diff

```
detect_changes({ scope: "staged" })
```

Or for all uncommitted work:

```
detect_changes({ scope: "working" })
```

Or against a base branch:

```
detect_changes({ scope: "base_ref", base_ref: "main" })
```

Returns:
```json
{
  "changed_files": ["src/main/.../OrderService.java", ...],
  "changed_symbols": [{ "id": "Method:...", "kind": "Method", "name": "...", "file": "..." }, ...],
  "affected_symbols": ["Method:com.acme.PaymentService#charge", ...],
  "affected_processes": ["Process:CreateOrder", ...],
  "risk": "high"
}
```

**Risk tiers:**
| `risk` | `affected_symbols` count |
|--------|--------------------------|
| `none` | 0 |
| `low` | 1–5 |
| `medium` | 6–20 |
| `high` | 21–75 |
| `critical` | 76+ |

#### Step 2 — Deep-dive the most-affected symbols

For each symbol in `affected_symbols` that looks concerning:

```
context({ name: "<affected_symbol_id>" })
```

Inspect `callers` to understand the call stack depth; inspect `processes` to see which
named business flows are at risk.

---

### Option B — Symbol-specific (when you know what you're changing)

#### Step 1 — Look up the symbol

```
query({ q: "OrderService save" })
```

Take the `node_id` from the top hit.

#### Step 2 — Run upstream blast-radius

```
impact({ name: "Method:com.acme.OrderService#save", direction: "upstream", max_depth: 4 })
```

Returns:
```json
{
  "root": "Method:com.acme.OrderService#save",
  "direction": "upstream",
  "affected": [{ "id": "Method:...", "depth": 1, "via": "CALLS" }, ...],
  "risk": "medium"
}
```

`depth` is the BFS hop count from the root. Symbols at depth 1 call the changed method
directly; depth 2 call those callers; etc.

#### Step 3 — Narrow to downstream (what this method depends on)

```
impact({ name: "Method:com.acme.OrderService#save", direction: "downstream", max_depth: 3 })
```

Use `downstream` to understand dependency chain — useful before deleting a utility.

#### Step 4 — Get process membership

```
context({ name: "Method:com.acme.OrderService#save" })
```

Inspect `processes` — if a symbol participates in a critical process (e.g. "RefundPayment"),
flag it in the analysis even if the raw symbol count is low.

---

## Disambiguation

If you pass a short name and get an ambiguous response:

```json
{ "status": "ambiguous", "candidates": [
    { "id": "Method:com.acme.OrderService#save", "kind": "Method", "name": "save", "file": "..." },
    { "id": "Method:com.acme.CartService#save",  "kind": "Method", "name": "save", "file": "..." }
]}
```

Present the list to the user and ask which symbol they mean, then retry with the full `id`.

---

## Output shape to return to the user

```json
{
  "changed_symbols": ["<kind>:<fqn>", ...],
  "affected_count": 42,
  "risk": "high",
  "hot_spots": ["<fqn> (depth <n>, process: <process_name>)", ...],
  "affected_processes": ["<process_name>", ...],
  "recommendation": "<one sentence: safe to proceed / needs review / high-risk>"
}
```

---

## Tips

- `detect_changes` is the fastest path for PR review — one call replaces several `impact` calls.
- Always check `processes` in `context` for the root symbol; a "low" risk score can hide a
  critical business process if the affected count is small but the process is payment-critical.
- Use `max_depth: 6` for library utilities that may have deep transitive callers.
- `direction: "both"` shows the full call neighbourhood but is noisy; prefer explicit
  `upstream` for blast radius and `downstream` for dependency analysis.
- Cross-repo blast radius: if the repo belongs to a group, `status` lists each group
  with `contracts_synced_at` and `stale`. When `stale` is true (a member repo was
  re-indexed since the last sync), `api_impact`/`group_contracts` results may miss or
  misreport consumers — re-run `cih-engine group sync <group>` first. Contract tool
  responses carry the same `contracts_synced_at`/`contracts_stale` fields.
