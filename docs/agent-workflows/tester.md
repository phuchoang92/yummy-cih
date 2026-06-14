# CIH Workflow: Tester / QA Engineer

**Persona:** Tester, QA Engineer, SDET
**Goal:** Scope regression testing for a change — identify which symbols were modified, which
callers are affected, and which business processes need end-to-end verification.

---

## When to use this workflow

- "Which test suites should I run after this PR?"
- "What is the regression scope of merging this branch?"
- "Which business processes are touched by this diff?"
- "Find all tests that exercise the payment flow"

---

## Step-by-step

### Step 1 — Identify changed symbols from the diff

```
detect_changes({ scope: "staged" })
```

For a PR branch vs main:

```
detect_changes({ scope: "base_ref", base_ref: "main" })
```

Returns:
```json
{
  "changed_files": ["src/main/.../PaymentService.java"],
  "changed_symbols": [
    { "id": "Method:com.acme.PaymentService#charge", "kind": "Method", "name": "charge", "file": "..." }
  ],
  "affected_symbols": ["Method:com.acme.OrderService#completeOrder", ...],
  "affected_processes": ["Process:CreateOrder", "Process:RefundPayment"],
  "risk": "high"
}
```

Use `affected_processes` as the list of business flows that need end-to-end test coverage.
Use `affected_symbols` as the list of methods whose unit tests should be verified green.

### Step 2 — Find existing tests for the changed symbols

For each symbol in `changed_symbols`:

```
impact({ name: "Method:com.acme.PaymentService#charge", direction: "upstream", max_depth: 5 })
```

Scan `affected` for nodes whose `file` contains `Test`, `Spec`, or `IT` — these are likely
test classes or integration test methods that directly or transitively exercise the changed symbol.

You can also search by keyword:

```
query({ q: "PaymentService test charge" })
```

Look for hits in files matching `*Test.java`, `*IT.java`, or `*Spec.java`.

### Step 3 — Understand the full blast radius

```
context({ name: "Method:com.acme.PaymentService#charge" })
```

Returns `callers` (direct) and `processes`. Cross-reference `callers` against known test
entry points to assess what is already covered by existing tests.

For the deepest blast-radius view:

```
impact({ name: "Method:com.acme.PaymentService#charge", direction: "upstream", max_depth: 6 })
```

Symbols at depth 1 are direct callers — their unit tests are the highest-priority regression
suite. Symbols at depth 3+ are indirect callers — prefer integration tests for those.

### Step 4 — Map changed files to test suites

For each file in `changed_files`, find the corresponding test class by naming convention:
- `src/main/.../PaymentService.java` → `src/test/.../PaymentServiceTest.java`

Verify these files exist and are included in the build before the PR merges.

### Step 5 — Identify affected processes needing E2E coverage

From `detect_changes`, `affected_processes` lists named business flows at risk.
Read the full process catalogue:

```
cih://repo/{name}/processes
```

Find each affected process by name. Its `handler` field gives the entry-point method id —
this is typically the controller method that should be covered by an E2E or API test.

Look up the route for each handler:

```
route_map({ prefix: "/api" })
```

Match `handler_id` against the process entry point to get the HTTP method + path for the
E2E test case.

### Step 6 — Score coverage confidence

Combine the signals:

| Signal | Weight |
|--------|--------|
| Direct test callers found in `impact` upstream | High |
| Process has an associated route (testable via HTTP) | High |
| Affected symbols are in a high-cohesion community | Medium |
| `risk` tier from `detect_changes` | Context |

Recommend the minimum test scope:
- **Unit tests:** for every method in `changed_symbols`
- **Integration tests:** for every method at depth ≤ 2 in `affected_symbols`
- **E2E tests:** for every process in `affected_processes` that has an HTTP entry point

---

## Quick regression scope for a hotfix

When a hotfix is being pushed urgently:

```
detect_changes({ scope: "staged" })
```

If `risk == "low"` and `affected_processes` is empty → unit tests of `changed_symbols` are
sufficient. If `risk == "medium"` or higher, or `affected_processes` is non-empty → require
at least one E2E pass through each affected process before merge.

---

## Output shape to return to the user

```json
{
  "changed_symbols": ["Method:com.acme.PaymentService#charge"],
  "risk": "high",
  "recommended_tests": {
    "unit": ["PaymentServiceTest#testCharge", "..."],
    "integration": ["OrderServiceIT#testCompleteOrder"],
    "e2e": ["POST /api/orders (CreateOrder process)", "POST /api/refunds (RefundPayment process)"]
  },
  "affected_processes": ["CreateOrder", "RefundPayment"],
  "coverage_confidence": "medium — 2 of 3 affected processes have known test callers"
}
```

---

## Tips

- `detect_changes` is the single most useful tool for regression scoping — run it on every
  PR before deciding test depth.
- Test class names in Java follow the `*Test`, `*IT`, `*Spec` convention; filter `impact`
  results by file suffix to find them.
- A `risk: "critical"` result (76+ affected symbols) almost always means a shared utility was
  changed — escalate to the lead and require full regression before merge.
- If `affected_processes` includes a payment or authentication process, always recommend
  E2E regardless of the raw symbol count.
- `communities` can help estimate blast radius at a module level — if two or more communities
  appear in `affected_symbols`, the change crosses module boundaries and needs cross-team sign-off.
