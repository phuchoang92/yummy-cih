---
name: cih-testing
description: Scope regression, integration, and end-to-end tests from a code change.
---

# Regression testing with CIH

Use this workflow to decide which unit, integration, and end-to-end tests a change requires.

1. Run `detect_changes` for the staged diff or comparison base.
2. Treat changed symbols as unit-test targets and depth-1/2 dependents as integration scope.
3. Use upstream `impact` with tests included to find existing test callers.
4. Map affected processes to their handlers and routes for end-to-end scenarios.
5. Use test-coverage tools when available and identify explicit coverage gaps.

Require at least one end-to-end pass for each affected authentication or payment process.
When impact crosses functional areas or is HIGH/CRITICAL, recommend broader regression and
cross-team review. Do not infer coverage solely from test-file naming conventions.
