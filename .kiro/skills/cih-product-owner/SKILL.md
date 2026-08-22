---
name: cih-product-owner
description: Understand APIs, business processes, and functional areas without reading all code.
---

# Product and business analysis with CIH

Use this workflow to explain what a service does without requiring the reader to inspect code.

1. Check repository freshness and whether route/community/process data is current.
2. Use the route map to catalogue the HTTP surface and group endpoints by business prefix.
3. Read communities to identify functional areas, size, and cohesion.
4. Read processes to identify named business flows and their entry points.
5. Use `context` on a handler to explain the domain services and downstream effects.
6. Use API impact or change detection to scope proposed sprint work.

Translate internal node IDs into HTTP method + path, functional-area names, and business-flow
language. Call out stale or absent discovery data rather than treating zero results as proof
that a feature does not exist.
