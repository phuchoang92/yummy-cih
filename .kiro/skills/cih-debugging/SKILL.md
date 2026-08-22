---
name: cih-debugging
description: Trace failures through callers, callees, and execution flows with CIH.
---

# Debugging with CIH

Use this workflow to trace an error, unexpected behavior, or failing operation.

1. Capture the concrete symptom, entry point, error text, and reproduction boundary.
2. Use `query` with the error and domain concept to find likely flows and definitions.
3. Use `context` on the failing or suspected symbol to inspect incoming and outgoing edges.
4. Use `trace` when both the source and destination symbols are known.
5. Read the process resource and source around the relevant steps; verify guards and data shape.
6. Once the root cause is identified, run upstream `impact` before proposing a code change.

Separate observed facts from hypotheses. A missing graph edge is not proof that reflection,
framework dispatch, generated code, or configuration cannot connect two components.
