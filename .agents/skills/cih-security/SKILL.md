---
name: cih-security
description: Review source-to-sink taint paths and scope security fixes with CIH.
---

# Security review with CIH

Use this workflow to investigate user-controlled data reaching SQL, command execution, file,
template, or other sensitive sinks.

1. Enumerate persisted taint findings for the repository or target area.
2. Refine by category and inspect the complete ordered source-to-sink path.
3. Read the source and guards around both endpoints and every important hop.
4. Use `context` on the sink and upstream `impact` to scope every reachable entry point.
5. Check regression coverage before recommending a fix.

Prioritize command/code injection, SQL injection, path traversal, and XSS findings by evidence
and reachability. CIH analysis has known blind spots around callbacks, reflection, properties,
and context-sensitive same-name callees; absence of findings is not proof of safety.
