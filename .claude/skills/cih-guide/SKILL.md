---
name: cih-guide
description: Use CIH MCP tools, graph resources, and code-intelligence workflows correctly.
---

# CIH Guide

Start with repository status and freshness. Re-index stale repositories before drawing
conclusions from the graph.

1. Use `query` to find relevant execution flows and definitions.
2. Use `context` for a symbol's callers, callees, and process membership.
3. Use `impact` in the upstream direction before editing an existing symbol.
4. Use `detect_changes` before committing to map the diff to affected flows.
5. Use route, API-impact, test-coverage, and taint tools for their specialized questions.

Prefer exact symbol IDs when a short name is ambiguous. An absent graph relationship is
evidence-limited, not proof that runtime coupling cannot exist.
