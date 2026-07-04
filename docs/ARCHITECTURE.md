# Architecture — parser assumptions & known limitations

CIH builds its graph from tree-sitter parses plus a set of framework/SQL
heuristics. The heuristics are deliberately conservative: when a fact can't be
established statically, CIH prefers to emit nothing (or mark it uncertain) rather
than guess. This page documents the assumptions so that answers built on the
graph — impact, route_map, taint_paths — can carry the right caveats.

For the full pipeline overview see `README.md`. This page is only the "where the
graph can be incomplete" list.

## HTTP routes (Java / Spring, `cih-lang/src/java/parse/framework.rs`)

- **Only the five `@*Mapping` shortcuts are treated as verbs**: `@GetMapping`,
  `@PostMapping`, `@PutMapping`, `@DeleteMapping`, `@PatchMapping`. A method
  annotated only with `@RequestMapping(method = RequestMethod.POST)` produces **no
  Route node**. `@RequestMapping` at the *class* level is still honored as the path
  prefix. (Pinned by `method_level_request_mapping_emits_no_route` in
  `cih-lang/tests/java.rs`.)
- **Path composition** trims and collapses slashes: class prefix `"/owners/"` +
  method `"/{id}"` → `/owners/{id}`; a bare `@GetMapping` under a class prefix
  resolves to the prefix alone. Multiple paths in one annotation
  (`@GetMapping({"/a","/b"})`) emit one Route each.
- **Feign clients**: `@FeignClient` URL/path is read from the annotation literally;
  dynamic URL interpolation (`${...}`, concatenation) is not followed.

## SQL / DB access (`cih-parse/src/sql.rs`, `cih-resolve/src/db_access.rs`)

- **Table extraction is textual** over the SQL string: it handles SELECT/INSERT/
  UPDATE/DELETE/MERGE, JOINs, comma-joins, sub-queries (including nested), UNION,
  schema-qualified names, and Oracle hint/line comments. `DUAL` is ignored.
  `INSERT ... SELECT` records the target as a write and the source as a read.
- **DB-constant resolution is same-file / same-class only.** A SQL string assembled
  from constants defined in another class is not resolved to its tables.
- **Dynamic SQL is not table-resolved.** When a query is built at runtime from
  non-literal parts, the DbQuery node is marked `dynamic = true` and **no table
  edges** are emitted. Taint analysis still treats such dynamic execution as a
  potential `sql` sink — absence of a table edge is not absence of risk.

## Call graph (`cih-resolve`)

- Calls are resolved by receiver type + import/scope binding. **Reflection,
  runtime dynamic dispatch through framework proxies, and calls through
  string-named beans can be missed.** Interface calls resolve to declared
  implementors found in the indexed scope; implementors outside the indexed
  modules are not linked.

## Implications for agents

- A clean `taint_paths` result (or an empty `route_map` prefix) means "nothing
  found under these heuristics," **not** a proof of absence. Security and
  completeness summaries should say so.
- If a codebase relies heavily on `@RequestMapping(method=...)`, reflection, or
  cross-class dynamic SQL, expect the graph to under-report those specific edges.
  Custom sinks/sanitizers can be added via `cih.taint.toml` (see
  `docs/agent-workflows/security.md`).
