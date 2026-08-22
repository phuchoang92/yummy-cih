---
name: cih-exploring
description: Explore an unfamiliar codebase, architecture, and execution flows with CIH.
---

# Exploring a codebase with CIH

Use this workflow when asked how a feature works, where logic lives, or how the
architecture is connected.

1. Check the repository status and index freshness. Re-run `cih index .` if stale.
2. Use `query` with the user's concept to find ranked execution flows and definitions.
3. Use `context` on the most relevant exact symbol for callers, callees, and process membership.
4. Read the matching process resource for the ordered execution trace.
5. Read only the source ranges needed to confirm behavior.

Prefer graph results over broad text search for relationships. If a short symbol name is
ambiguous, present the candidates or retry with its exact ID and file path. Explain where
graph evidence ends; reflection and runtime wiring may not be fully represented.
