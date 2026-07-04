# Plan: CFG / PDG + Taint Analysis for cih

## Background

### Control Flow Graph (CFG)
A method body becomes a graph of **basic blocks** — linear sequences of statements with no branches. Edges represent how execution moves between them: sequential, conditional branch (if/else), loop back-edges, exception paths (try/catch/finally). Every statement lives in a basic block; every branch creates a fork in the graph.

### Program Dependence Graph (PDG)
Built on top of the CFG. Has two kinds of edges:
- **Control dependence**: statement B is control-dependent on A if A's outcome determines whether B executes (e.g., the body of an `if` is control-dependent on the condition)
- **Data dependence**: statement B reads a value written by statement A (def-use chain)

PDG enables **program slicing** — "find every statement that could affect this variable at this point." It's the backbone of refactoring tools, dead-code detection, and taint tracking.

### Taint Analysis
Uses the PDG to track *untrusted data*:
1. **Sources** — where tainted data enters (e.g., `@RequestParam`, HTTP body, env vars, stdin)
2. **Propagation** — follows def-use edges; taint spreads through assignments, arithmetic, string concat, and method calls
3. **Sinks** — where tainted data must not arrive unchecked (SQL query, `exec()`, file path, HTML output)
4. **Sanitizers** — functions that remove taint (e.g., `escapeHtml()`, `parameterize()`)

A taint path from source → propagation chain → sink is a vulnerability finding (SQL injection, XSS, path traversal, command injection).

---

## Current cih IR — Gap Analysis

### What cih already has (inter-procedural level)
| Capability | Detail |
|---|---|
| Call graph | `CALLS` edges between methods |
| Type graph | `Extends`, `Implements`, `HasMethod`, `HasField` |
| Field access | `Accesses` edges (read/write, method-level) |
| Receiver types | `TypeBinding` (params, locals, call-results per method) |
| Complexity counts | `ComplexityRecord`: cyclomatic, if/loop/try/throw counts per method |
| Entry points | Source/sink annotations (`contract_sites`) via `@RequestParam`, `@KafkaListener`, etc. |
| Call site args | `arg_texts` captured at parse time (up to 120 chars) |

### What's missing (intra-procedural level)
| Missing Piece | Why Needed |
|---|---|
| Statement-level IR | Basic blocks require individual statements, not just call sites |
| CFG edges within method | Branch/loop/exception successors |
| Def-use chains | Data-dependence edges for PDG |
| Dominance tree | Control-dependence computation requires dominator analysis |
| Local variable assignment tracking | Taint propagation must follow each write |

**The parsers already have full AST access** (tree-sitter) — the current `parse.rs` files simply don't extract statement-level info. The tree is there; it's just not walked at statement depth.

---

## Feasibility Verdict

**Yes — but it's a large, phased build.** The architecture is sound: tree-sitter gives full ASTs, the graph store can hold statement nodes and PDG edges, and the existing call graph provides the inter-procedural backbone. The work is in adding an intra-procedural layer.

Good news: **a useful 80% of taint analysis value is achievable without full CFG/PDG**, by using inter-procedural light taint on the existing call graph + source/sink patterns already present in `contract_sites`.

> **Precision caveat**: method-level taint (Phase 0) has no argument tracking, which means it will flag any method reachable from a source that reaches a sink — even if the tainted value is never actually passed through. Set team expectations on false positive rate early. Phase 0 value is in surfacing suspicious chains for human review, not zero-FP alerts.

---

## Performance & Efficiency Strategy

### The problem with eager CFG/PDG

Building CFG/PDG eagerly across the full corpus is prohibitively expensive:

- **Parse overhead**: walking tree-sitter ASTs to statement depth adds ~2–4× current Java parse time
- **Storage blowup**: ~100k methods × ~20 statements = ~2M `StatementNode`s + 4–6M CFG edges + up to 4M PDG edges in Phase 3 — 3 orders of magnitude more than the current method-granularity graph
- **Query noise**: statement-level subgraphs bloat every traversal that the chat/wiki/MCP tools run, even for queries that never need intra-procedural detail

### Demand-driven CFG/PDG (adopted approach)

**Build CFG/PDG only when triggered.** The main graph stays method-granularity permanently. CFG/PDG is computed on demand, results cached per-method, and either held in memory for the lifetime of the request or stored in a sidecar (not the main graph).

**Trigger conditions** (any one is sufficient):
1. The method belongs to an API entry point — identified via `contract_sites` (`@RequestMapping`, `@KafkaListener`, `@RabbitListener`, etc.)
2. Phase 0 BFS places the method on a taint path — only methods in the path get promoted to intra-procedural analysis
3. An external caller requests it explicitly — MCP tool call (`analyze_method(fqn)`), chat query naming a specific method, or wiki page load for that method

**How Phase 0 and demand-driven CFG/PDG compose:**

```
Phase 0 BFS on CALLS edges finds candidate path:
  HTTP endpoint A → service B → JDBC sink C

  → enqueue A, B, C into CfgRequestQueue
  → CFG/PDG pass runs only for those 3 methods (in-memory)
  → follow DataDep edges intra-procedurally within each method
  → taint actually flows through? emit TaintFlow edge; else discard
  → evict CFG/PDG from memory after analysis
```

Phase 0 is the cheap filter. CFG/PDG is the precision pass on the small subset that matters. On a 100k-method codebase, the taint-reachable set is typically a few hundred methods — 3–4 orders of magnitude less work than full-corpus analysis.

### CfgRequestQueue (implementation sketch)

A `CfgRequest` queue lives in `cih-engine`. Methods enter it via:
- `taint.rs` — any method on a Phase 0 taint path
- MCP tool handler — `analyze_method(fqn)` tool call
- wiki renderer — on-demand when a method page is loaded

The CFG/PDG worker pulls from the queue, computes the intra-procedural graph **in memory**, runs the relevant analysis (taint precision pass, slice query, dead-block detection), emits only the *derived results* (refined `TaintFlow`, slice summary, dead block annotation) into the main graph, then discards the statement IR.

**Cache invalidation**: each method's CFG/PDG result is keyed by `(fqn, ast_hash)`. If the method body changes on re-index, the hash changes and the entry is evicted. No stale analysis served.

### What never goes into the main graph

- `StatementNode`s
- `BasicBlock` nodes
- `CfgSuccessor` / `CfgBranch` / `CfgException` edges
- Raw `ControlDep` / `DataDep` edges

These live only in the CFG/PDG worker's in-memory representation. The main graph receives only the distilled outputs.

---

## Proposed Phases

### Phase 0 — Inter-procedural "light taint" (immediate, ~1–2 weeks)
**No new IR required.** Uses the existing call graph + `contract_sites` + `arg_texts`.

**What to build:**
- A `taint` analysis pass in `cih-resolve` (or new `cih-taint` crate)
- Source rules: match nodes with `contract_sites` kind = HTTP/event (already tagged)
- Sink rules: match nodes that produce `CALLS` edges to known dangerous methods (e.g., `jdbcTemplate.execute`, `Runtime.exec`, `Files.write`)
- Sanitizer rules: stop propagation at known sanitizer FQCNs
- Propagation: BFS/DFS on `CALLS` edges from source methods to sink methods
- Output: `TaintPath` — list of method-level hops from source to sink, with `arg_texts` at each call site for context
- Store as new edge kind `TaintFlow` in the graph

**Value**: Finds inter-method taint paths (e.g., "this HTTP endpoint → payment service → raw SQL call") without any intra-procedural analysis. Good for 60–70% of real security issue surface area.

**Files to add**: `crates/cih-resolve/src/taint.rs` (or new `cih-taint` crate)  
**Files to modify**: `cih-core/src/lib.rs` (add `TaintFlow` edge kind), `cih-engine/src/analyze.rs` (call the new pass)

#### Scoping decisions to make before Phase 0 starts

**1. `TaintFlow` storage semantics** — choose one:
- One edge per hop (A→B, B→C = two `TaintFlow` edges). Pollutes the graph with derived data; hard to query the full path.
- One edge source→sink with the path encoded as a property (`hops: ["A","B","C"]`). Loses traversability in graph queries.
- Recommended: **one edge source→sink with `hops` property** for the initial implementation; upgrade to hop-level if traversal queries demand it.

**2. Sanitizer model limitations** — The FQ-classname stop rule works for output encoders (`escapeHtml`, `HtmlUtils.htmlEscape`). It does **not** work for SQL parameterization because the sanitizer is using `PreparedStatement` rather than calling a named sanitizer method. Phase 0 should explicitly document this gap so the FN rate on SQL injection is understood upfront.

**3. FP mitigation** — Before shipping, add a lightweight arg-text heuristic: when propagating through a CALLS edge, check whether `arg_texts` at the call site contains a variable name that matches a tracked tainted parameter. This is imprecise but cuts obvious non-flows and reduces noise materially.

**4. Wiki output spec** — Define what a developer sees before wiring into `cih-wiki`:
  - Taint path list (source method → ... → sink method)
  - Hop count + call site `arg_texts` at each hop
  - Sink category (SQL, exec, file, HTML, ...)
  - Severity (high/medium based on sink type and hop count)

---

### Phase 1 — On-demand statement-level IR (large, per-language)
Add a `StatementNode` to an **in-memory-only** IR used by the CFG/PDG worker. **Start with Java only**; TypeScript/Python follow once the schema is validated. Statement nodes are **never persisted** to the main graph.

**Estimated effort**: Java statement IR = 4–6 weeks; full multi-language = 2–3 months

In-memory IR types (live in `cih-taint` worker, not `cih-core/src/ir.rs`):
```rust
pub struct StatementNode {
    pub id: NodeId,
    pub kind: StatementKind,  // Assign, Call, Return, Branch, Loop, Throw, ...
    pub in_callable: NodeId,  // owning method
    pub range: Range,
    pub reads: Vec<String>,   // variable/field names read
    pub writes: Vec<String>,  // variable/field names written
    pub call_site: Option<NodeId>,  // links to ReferenceSite if kind == Call
}
```

The Java parser (`cih-lang/src/java/parse.rs`) already walks deep into method bodies for SQL/contract extraction — the tree-sitter AST is already available. Phase 1 adds a second walk that emits `StatementNode`s into the worker's in-memory graph, triggered only when the method is in the `CfgRequestQueue`.

No `NodeKind` or `EdgeKind` additions to `cih-core` are needed in Phase 1 — all new node/edge types are internal to the `cih-taint` worker.

---

### Phase 2 — CFG construction (medium, build on Phase 1)
In the `cih-taint` worker, after statement IR is built for a requested method:
1. Group `StatementNode`s into basic blocks (split at branches and join points)
2. Build successor edges between basic blocks in memory
3. Handle conditional branches (true/false targets) and exception edges (try→catch)
4. Compute dominance tree (Cooper-Harvey-Kennedy algorithm on the in-memory CFG)

Nothing is written to the main graph. The CFG lives only for the duration of the analysis request.

**Files**: new `crates/cih-taint/src/cfg.rs`

---

### Phase 3 — PDG + full taint (complex, build on Phase 2)
In the `cih-taint` worker:
1. Compute control-dependence edges from dominance tree (in memory)
2. Compute data-dependence edges from reaching-definition analysis (in memory)
3. Run statement-level taint: follow `DataDep` edges rather than `CALLS` edges
4. Add sanitizer annotation support (per-language config in `entry_points/`)
5. Emit only the refined `TaintFlow` result into the main graph (source→sink edge with `hops` + `confidence` properties)

**Files**: new `crates/cih-taint/` crate containing `cfg.rs`, `pdg.rs`, `taint.rs`, `queue.rs`

---

## Recommended Start

Implement **Phase 0** now. It gives real security value (inter-method taint paths), reuses 100% of the existing IR, and is achievable in 1–2 weeks. Phases 1–3 are scoped for later once the value of taint output is validated with real users.

**Phase 0 implementation steps:**

1. Add `TaintFlow` to `EdgeKind` in `cih-core/src/lib.rs`
2. Create `crates/cih-resolve/src/taint.rs` with:
   - `TaintSource` / `TaintSink` / `TaintSanitizer` config structs (source from `entry_points/*.toml` or hardcoded initial rules)
   - `find_taint_paths(parsed, call_graph, sources, sinks) -> Vec<TaintPath>` function
   - BFS on `CALLS` edges; stop at sinks or sanitizers; record path + `arg_texts`
   - Lightweight arg-text heuristic to filter obvious non-flows (see Phase 0 notes above)
3. Emit `TaintFlow` edges (source→sink, `hops` property) into the graph
4. Add a `taint` subcommand to `cih-engine` that runs after resolve
5. Wire into `cih-wiki` to surface taint paths in the dev pages (use the output spec above)

---

## Verification

**Unit tests** (in `crates/cih-resolve/tests/` or `cih-taint`):
- Controller → service → JDBC raw SQL, no sanitizer → taint path found
- Controller → service → parameterized query (`PreparedStatement`) → no path (or path flagged as lower severity)
- Controller → sanitizer method → sink → no path
- Multi-hop path (controller → service → repo → SQL) → full hop chain captured in `hops` property

**Integration test**: run on [DVJA (Damn Vulnerable Java Application)](https://github.com/appsecco/dvja) or OWASP WebGoat. Verify findings match their documented injection points. This is the real validator — unit tests on synthetic fixtures are insufficient to measure FP/FN rate.

**Acceptance criteria for Phase 0 ship**:
- At least one known SQLi path in the integration target is found
- FP rate on a clean Spring Boot CRUD app (parameterized queries throughout) is < 20% of paths flagged
