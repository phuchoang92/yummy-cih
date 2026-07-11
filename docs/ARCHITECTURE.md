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

## Kotlin routes & contract sites (`cih-lang/src/kotlin/framework.rs`)

A 1:1 port of the Java Spring/Feign/Kafka detector (string normalization is
shared via `cih-lang/src/contracts_common.rs`; tree walking is per-language).
Same verb-shortcut and path-composition rules as Java above, plus:

- **Receiver typing is a light per-class env**: a call like
  `restTemplate.getForObject(...)` only counts as an outbound HTTP contract when
  the receiver's simple name matches a *typed primary-constructor parameter* or
  a *typed property* of the enclosing class (`class C(private val rest:
  RestTemplate)`). No inheritance, no local variables, no `this.`-qualified
  chains — an untyped or externally-provided receiver emits nothing.
- **Literal strings only (Phase A)**: an interpolated URL (`"$base/items/$id"`)
  still emits the HTTP contract site but with no `url_template`; an interpolated
  topic in `kafkaTemplate.send(...)` emits nothing. Neither can participate in
  cross-repo matching until dynamic-URL folding lands.
- **Top-level `fun`s** get contract sites (as `Function:` callables); calls in
  `init {}` blocks and property initializers have no callable context and are
  skipped, mirroring Java.

## CXF / OSGi route stitching (`cih-resolve/src/lang/java/cxf.rs`)

- **Base paths come from XML, per bundle.** Each JAX-RS route gets
  `servlet_prefix + <jaxrs:server address> + method_path`. The servlet prefix is
  resolved per server file: the OSGi whiteboard pattern
  (`osgi.http.whiteboard.servlet.pattern`) whose declaring XML shares the most
  leading directory components wins; a lone repo-wide pattern applies everywhere;
  multiple unrelated patterns are never guessed across bundles
  (`servlet_prefix_source: "none"`; `cxf_base_path` in `cih.toml` overrides all).
- **Interface-annotated endpoints stitch via heritage.** When the `jaxrs:server`
  bean is an impl class but the route handler is the annotated interface (JAX-RS
  annotation inheritance, common on OSGi platforms with `-api` bundles), the
  target matches any interface the bean class transitively implements. Exact
  impl-class matches always take priority.
- **One route per server address.** A handler matched by several servers with
  distinct addresses (secured `/v1` + non-secured `/ns/v1` impls of one
  interface) yields one Route node per resulting path — the first rewrites in
  place, further addresses are cloned with duplicated incoming edges. Route
  counts on such platforms intentionally reflect every real URL.
- **Spring-DM OSGi wiring is DI input.** `META-INF/spring/*.xml` files (any
  name) are DI-XML candidates; `<osgi:reference interface=…>` produces the same
  interface→implementor `CALLS` edges as Blueprint `<reference>` (reason string
  `di-xml-blueprint-reference` is kept for compatibility and covers both).

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

## Performance notes

- **Symbol-ID interning: measured, not needed (2026-07).** NodeId-keyed maps in
  the taint pipeline use `FxHashMap` (rustc-hash). A full 4-phase `taint` run on
  Fineract (~46k nodes) completes in ~0.77s wall time, so interning NodeIds to
  `u32` symbols in the PDG/dataflow hot path was evaluated and rejected — the
  string-keyed maps are not a bottleneck at real-repo scale. Re-evaluate only if
  a profile of a much larger repo shows hashing in the hot path.

## Implications for agents

- A clean `taint_paths` result (or an empty `route_map` prefix) means "nothing
  found under these heuristics," **not** a proof of absence. Security and
  completeness summaries should say so.
- If a codebase relies heavily on `@RequestMapping(method=...)`, reflection, or
  cross-class dynamic SQL, expect the graph to under-report those specific edges.
  Custom sinks/sanitizers can be added via `cih.taint.toml` (see
  `docs/agent-workflows/security.md`).
