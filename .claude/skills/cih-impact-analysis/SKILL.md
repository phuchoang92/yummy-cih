---
name: cih-impact-analysis
description: Assess the upstream blast radius and affected processes before changing code.
---

# Impact analysis with CIH

Use this workflow before changing an existing function, method, class, or public contract.

1. Run `impact` in the upstream direction on the exact symbol.
2. Review depth 1 first: these are direct callers or importers most likely to break.
3. Review affected execution processes and functional areas, not only the symbol count.
4. Warn before proceeding when the result is HIGH or CRITICAL.
5. After editing, run `detect_changes` and compare the observed scope with the intended scope.

Risk guide: fewer than five dependents is usually LOW; 5–15 or several processes is MEDIUM;
more than 15 or broad cross-module fan-out is HIGH. Authentication, payments, security, and
public API paths may be CRITICAL even with a smaller raw count. Include test symbols when
scoping regression coverage.
