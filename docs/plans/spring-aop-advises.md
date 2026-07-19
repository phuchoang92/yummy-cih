# Spring AOP Resolution — `ADVISES` Edges Plan

**Goal:** yummy-cih currently stores `@Aspect` / `@Around` / `@Before` / etc. as raw annotation
metadata but never interprets them. This plan adds a post-resolve pass that parses the common
pointcut-expression subset, matches it against Method nodes already in the graph, and emits
`Advises` edges so `trace_flow`, the wiki, and viz can surface interception.

**Repo:** `~/BigMoves/AI/yummy-cih` (branch `dev`).

---

## Current state (verified 2026-07-19)

- Every annotation on a class/method — simple name + string-literal attrs (positional `value`,
  `key = "..."` pairs, string arrays) — is retained on node props as `annotations` by
  `annotation_metadata` (`crates/cih-lang/src/java/parse/mod.rs:187`, attached at
  `parse/declarations.rs:178` for callables and `parse/structural.rs:168` for types).
  So `@Aspect`, `@Around("execution(...)")`, `@Pointcut`, `@Order` are already on the graph —
  **no parser changes needed for the core cases.**
- The `cih.patterns.toml` engine (`crates/cih-resolve/src/patterns.rs`) is the precedent for a
  post-resolve pass that matches annotation metadata and synthesizes nodes/edges. It runs in
  `crates/cih-engine/src/analyze/mod.rs:540`, after `resolvers.post_process(...)`.
- Method `qualified_name` format: `com.acme.Foo#pay/1` (FQCN `#` name `/` arity) — visible in
  `patterns.rs` handler parsing.
- `EdgeKind` (`crates/cih-core/src/lib.rs:238`) serializes SCREAMING_SNAKE_CASE via strum, so a
  new `Advises` variant becomes the `ADVISES` Cypher label with zero adapter work for label
  naming.
- Nothing anywhere parses pointcuts: zero hits for `pointcut|Around|advice` in `cih-resolve` /
  `cih-patterns`. Grouping only keyword-matches "aspect" for the cross-cutting group
  (`cih-grouping/src/strategies/structural.rs`).

## Scope

**In scope (phase 1–2):**
- Aspects declared as annotated classes: `@Aspect` + `@Around` / `@Before` / `@After` /
  `@AfterReturning` / `@AfterThrowing` advice methods.
- Pointcut designators: `execution(...)`, `@annotation(...)`, `within(...)`, `@within(...)`,
  `bean(...)` (name pattern), and named-pointcut references to `@Pointcut` methods **within the
  same aspect class**.
- Boolean combinators `&&`, `||`, `!` over the above.
- `@Order` on the aspect class → edge prop for precedence.

**Out of scope (documented as known gaps, revisit only if an eval repo needs them):**
- XML-configured aspects (`<aop:config>`), `@DeclareParents` / introductions, `args()` /
  `this()` / `target()` runtime designators (statically undecidable beyond type matching),
  cross-class named-pointcut references (`com.acme.Pointcuts.repoOps()`), AspectJ compile-time
  weaving of non-Spring classes.
- Implicit proxy semantics of `@Transactional` / `@Cacheable` / `@Async` — different mechanism,
  different edge meaning; see "Later" section.

## Design

### 1. New edge kind (cih-core)

Add `Advises` to `EdgeKind` (`crates/cih-core/src/lib.rs`) with a doc comment mirroring
`TaintFlow`'s style. Direction: **advice method → advised method**
(`com.acme.LoggingAspect#log/1 -ADVISES-> com.acme.OrderService#pay/2`).

Edge props:
- `advice_kind`: `around | before | after | after_returning | after_throwing`
- `pointcut`: the resolved (inlined) expression string
- `aspect_order`: i64 from `@Order`, absent if none

Check every exhaustive `match` on `EdgeKind` (grep `EdgeKind::` across crates; the compiler
will find the rest) — notably `cih-server/src/layout.rs:216` edge-class buckets and any
wiki/viz kind filters.

### 2. New module: `crates/cih-resolve/src/lang/java/aop.rs`

Sibling of `cxf.rs` / `di.rs`. Two parts:

**a. Pointcut expression parser.** Small hand-rolled recursive-descent parser (no new deps)
producing:

```rust
enum Pointcut {
    Execution { type_pattern: TypePat, name_pattern: NamePat, params: ParamsPat },
    AnnotationOnMethod(String),   // @annotation(com.acme.Loggable) — simple or FQCN
    Within(TypePat),              // within(com.acme.service..*)
    AnnotationOnType(String),     // @within(...)
    Bean(NamePat),
    NamedRef(String),             // loggableOps()
    And(Box<Pointcut>, Box<Pointcut>),
    Or(Box<Pointcut>, Box<Pointcut>),
    Not(Box<Pointcut>),
    Unsupported(String),          // args(), this(), target(), unknown designators
}
```

Pattern semantics to implement:
- `*` in name/type segments (single segment), `..` in package position (any depth) and in
  params position (any args), `+` suffix on type pattern (subtypes — see 2c).
- `execution` grammar handled: `[modifiers] ret-type-pattern [declaring-type-pattern.]name-pattern(params)`.
  Return type and modifiers are parsed but only `*` vs. exact is honored for matching v1
  (return types aren't reliably on Method nodes as FQCNs).
- `Unsupported` inside `&&` degrades to "ignore that conjunct, mark edge prop
  `approximate: true`"; `Unsupported` at top level or under `!`/`||` ⇒ skip the advice and log
  at `debug` (fail-soft, same philosophy as `cih.patterns.toml`).

**b. Graph pass** `pub fn emit_advises_edges(nodes: &[Node], edges: &mut Vec<Edge>)`:
1. Collect aspect classes: `Class` nodes whose `annotations` metadata contains `Aspect`.
2. For each, collect member methods (via `qualified_name` prefix `Fqcn#` — same trick
   patterns.rs uses; no need for HasMethod traversal) and split into `@Pointcut` definitions
   (name → parsed expr) and advice methods (kind + expr from the advice annotation's `value`
   attr — already extracted as string literal by `annotation_metadata`).
3. Inline `NamedRef`s from the same class's `@Pointcut` map (one level of recursion with a
   visited set; cross-class refs ⇒ Unsupported).
4. Match every candidate `Method` node (skip methods of the aspect class itself, skip
   constructors/test stereotypes) against each advice's pointcut:
   - Method side inputs: FQCN + simple name + arity from `qualified_name`; owning class's
     `annotations` (build the same `class_annotations` map patterns.rs builds); class
     stereotype/bean name for `bean()` (bean name default = decapitalized simple class name,
     or annotation `value` if present on `@Component`/`@Service`/...).
   - `@annotation` matching: compare simple name (metadata stores simple names); if the
     pointcut uses an FQCN, compare its last segment. Document the theoretical false-positive
     (two different annotations with the same simple name) as accepted.
5. Emit one `Advises` edge per (advice method, matched method), deduped.

**Guardrail:** cap matches per advice (e.g. 2 000) with a `warn` log — a `within(..*)`-style
pointcut on a big monorepo must not explode the edge set. Overly broad pointcuts are real in
the wild.

**c. Subtype matching (`+`) — phase 2.** Requires `Extends`/`Implements` edges; pass `edges`
in and build a child→ancestors map. Phase 1 treats `Foo+` as `Foo` plus a `approximate: true`
prop; phase 2 does the real closure.

### 3. Wiring (cih-engine)

Call it from the Java resolver's `post_process`
(`crates/cih-resolve/src/lang/java/mod.rs:74`) — it's Java-specific, and post_process already
receives `&mut nodes, &mut edges` for CXF path rewriting, so no engine-level change beyond a
`tracing::info!(advises_edges = n, aspects = m)` line. Runs before `apply_pattern_rules`
(fine — no interaction) and before artifact write, so both graph stores ingest the edges with
no store-side changes.

### 4. Surfacing

- **trace_flow:** add `ADVISES` awareness without polluting call paths. Do **not** add it to
  the variable-length traversal union at `cih-falkor/src/query.rs:673` (an aspect would become
  a hop on every route trace and drown the output). Instead: after computing a trace, run one
  batched query for `ADVISES` edges **into** the traced methods and attach them as an
  `intercepted_by` annotation on the affected hops. Mirror in `cih-ladybug` (A/B parity is a
  standing requirement — see graph-store decoupling work).
- **Wiki (`cih-wiki`):** aspect classes get an "Advises N methods across M classes" line;
  advised methods' pages get an "Intercepted by" list. Keep it to the render layer — data is
  plain edges.
- **Viz/layout (`cih-server/src/layout.rs`):** classify `Advises` into the same bucket as
  other non-call semantic edges (dashed styling can come later).

## Testing

1. **Parser unit tests** (in `aop.rs`): table-driven over expression → matches/non-matches,
   covering `..` packages, `*` names, arity/params wildcards, `&&`/`||`/`!`, named refs,
   unsupported degradation.
2. **Resolve integration test** (`crates/cih-resolve/tests/resolve.rs` pattern): fixture with
   a `LoggingAspect` (`@Around` on `execution(* com.acme.service.*.*(..))` + `@Pointcut`
   named ref + `@annotation(Loggable)` advice), a service, a controller, and a negative-case
   class outside the package. Assert exact `Advises` edge set + props.
3. **End-to-end evals** (measure-first discipline): re-index **Fineract** (heavy
   `@Transactional` but also real aspects) and check aspect count / edge count / spot-check
   correctness; note numbers in the runbook. OCB and 212ecom should be unaffected — assert
   zero `Advises` on repos without `@Aspect`.
4. **Store parity:** extend the existing A/B (Falkor vs. Ladybug) parity check to a graph
   containing `Advises` edges and the trace_flow `intercepted_by` output.

## Phases

| Phase | Deliverable | Acceptance |
|---|---|---|
| 1 | `EdgeKind::Advises`; parser for `execution`/`@annotation`/`within`/combinators; same-class named refs; wiring + logging; unit + integration tests | Fixture emits exact expected edges; unaffected repos emit zero; all existing tests green |
| 2 | `bean()`, `@within`, subtype `+` via heritage closure, match cap + `approximate` prop | Fineract re-index numbers recorded; no runaway edge counts |
| 3 | Surfacing: trace_flow `intercepted_by` (both stores), wiki sections, layout bucket | Parity test green; wiki page for an aspect renders the advises list |

## Later / explicitly deferred

- `@Transactional` / `@Cacheable` / `@Async` proxy semantics — if wanted, model as a *node
  prop* (`proxied_behaviors: ["transactional"]`) rather than edges; no aspect source exists to
  point an edge from.
- `cih.patterns.toml` extension for custom advice annotations (e.g. in-house `@Audited`
  meta-annotations) — natural follow-on once the matcher exists.
- Cross-class named pointcut libraries; XML `<aop:config>`.

---

## Status (2026-07-19): IMPLEMENTED (phases 1–3)

- `EdgeKind::Advises` (`ADVISES`), emitted from `JavaResolver::post_process`
  via `crates/cih-resolve/src/lang/java/aop.rs`.
- Beyond the plan: declared `returnType`/`paramTypes` are used for real
  positional type checks (fewer `approximate` edges), and AspectJ parameter
  bindings (`@annotation(withFlushMode)`) resolve through the advice method's
  parameter types — required for Fineract's `FlushModeAspect`.
- Surfacing: `trace_flow` `intercepted_by` (falkor + ladybug, contract-suite
  parity case), mermaid dashed advice annotations, wiki dev-page sections.
- Measured: corpus `java-spring-aop` (6/6 exact edges end-to-end,
  `crates/cih-engine/tests/aop_corpus.rs`); Fineract → 1 aspect, 17 ADVISES
  edges, all onto the exactly-two `@WithFlushMode` service classes; zero edges
  on aspect-free repos.
- Gaps documented in `docs/ARCHITECTURE.md` § Spring AOP.
