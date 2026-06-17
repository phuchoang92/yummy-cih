# CIH Agent Workflow Guides

These files are persona-specific skill guides for the yummy frontend AI agent.
Each covers: when to use the workflow, step-by-step tool calls with example inputs/outputs,
output shape to return to the user, and tips.

## Workflows

| File | Persona | Goal |
|---|---|---|
| [exploring.md](exploring.md) | Any | Orient to an unfamiliar codebase — modules, entry points, key symbols |
| [impact-analysis.md](impact-analysis.md) | Developer, Tech Lead | "What breaks if I change X?" — blast-radius scoring before a PR |
| [debugging.md](debugging.md) | Developer | Trace execution path of a bug — callers, call chain, entry point |
| [product-owner.md](product-owner.md) | PO, BA | Business view — API surface, named processes, module breakdown |
| [tester.md](tester.md) | Tester, QA | Regression scope for a diff — test suites to run, E2E coverage |

## Quick tool reference

| Tool | Used in |
|---|---|
| `list_repos()` | All workflows — confirm which repo is indexed |
| `route_map()` | exploring, product-owner, debugging, tester |
| `communities()` | exploring, product-owner, tester |
| `query()` | exploring, impact-analysis, debugging, tester |
| `context()` | exploring, impact-analysis, debugging, product-owner, tester |
| `impact()` | impact-analysis, debugging, tester |
| `detect_changes()` | impact-analysis, product-owner, tester |
| `trace_flow()` | debugging |
| `feature_map()` | exploring, product-owner |
| `cih://repo/{name}/processes` | exploring, product-owner, debugging, tester |
| `cih://repo/{name}/communities` | product-owner |

## When to use which workflow

```
User question                              → Workflow
─────────────────────────────────────────────────────
"What does this codebase do?"             → exploring
"What APIs exist?"                        → product-owner
"What is the order lifecycle?"            → product-owner (trace_flow)
"Where is X implemented?"                 → exploring (query + context)
"What breaks if I change PaymentService?" → impact-analysis
"Where is this exception thrown from?"    → debugging
"Which tests should I run for this PR?"   → tester
```
