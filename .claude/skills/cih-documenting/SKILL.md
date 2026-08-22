---
name: cih-documenting
description: Generate and verify architecture documentation grounded in CIH evidence.
---

# Architecture documentation with CIH

Use this workflow to create documentation grounded in the current graph rather than guesses.

1. Confirm the index and wiki are current for the repository HEAD.
2. Use repository context, communities, routes, and process resources to outline the system.
3. Use `context` for key entry points and shared services, then verify important source ranges.
4. Link claims to symbol IDs, file paths, routes, or process names as appropriate for the reader.
5. Generate or refresh the CIH wiki when a durable documentation bundle is requested.
6. Verify links and `.cih/wiki/agent-index.json` after generation.

Keep business descriptions separate from implementation detail, state evidence limitations,
and never invent an execution edge merely to make a narrative complete.
