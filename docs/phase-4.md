# Phase 4 — Scope resolution + MRO (detailed plan)

Goal: turn Phase 3's **unresolved `ReferenceSite`s** into a real, accurate call/heritage graph —
`CALLS` / `ACCESSES` / `USES` / `EXTENDS` / `IMPLEMENTS` / `METHOD_OVERRIDES` / `METHOD_IMPLEMENTS`
edges with `confidence` — loaded through the existing Phase-2 path. The headline acceptance:
`service.findOwner()` on a field `OwnerService service` resolves to `Method:…OwnerService#findOwner/1`,
so `impact()` returns real callers.

Builds on Phase 3: the persisted `ParsedFile` IR (`.cih/parsed/<version>/parsed-files.jsonl`) + the
structure graph already in FalkorDB. Ports from GitNexus
(`core/ingestion/scope-resolution/`): `pipeline/run.ts` (pass order), `passes/receiver-bound-calls.ts`
(the 7-case dispatcher), `graph-bridge/ids.ts` + `node-lookup.ts` (id lookup cascade),
`graph-bridge/references-to-edges.ts` (drain + drop-unresolved), `mro-processor.ts` (C3 MRO).

New crate: `cih-resolve`. Engine gains a resolve step after parse (or a standalone `resolve` subcommand).

---

## Why a prerequisite step (the IR is missing types)

Resolving `service.findOwner()` needs the **type of the receiver** `service`. Phase 3's scope query
already *captures* `@type-binding.*` (field/param/local/return types, incl. `var` inference), but
`cih-parse` only persisted imports + reference sites — **type bindings are dropped**, and `SymbolDef`
carries no declared/param/return type. So Phase 4 must start by enriching the IR.

### Phase 4.0 — Extend the IR (`cih-core` + `cih-parse`)

- `cih-core::ir`: add
  ```rust
  pub enum BindingKind { Param, Local, Field, CallResult, Alias, Pattern, Return }
  pub struct TypeBinding {
      pub name: String,         // bound identifier (the receiver name)
      pub raw_type: String,     // unresolved simple/raw type name
      pub kind: BindingKind,    // distinguishes param/local/field/var-call-result/alias/...
      pub in_fqcn: String,      // enclosing callable scope (lexical owner)
      pub range: Range,         // for nearest-binding / shadowing resolution
  }
  // SymbolDef gains, for callables/fields:
  pub param_types: Vec<String>,   // erased simple/raw names, ordered
  pub return_type: Option<String>,
  pub declared_type: Option<String>, // fields
  // ReferenceSite gains the caller's graph id directly (no string reconstruction):
  pub in_callable: NodeId,        // the edge SOURCE for resolved CALLS/ACCESSES (see F4)
  pub struct ParsedFile { …, pub type_bindings: Vec<TypeBinding> }
  ```
- `cih-parse`: in `collect_query_ir`, handle the `@type-binding.*` captures already produced by the
  query → push `TypeBinding { name, raw_type, kind, in_fqcn, range }`, mapping each capture to a
  `BindingKind` (`.parameter`→`Param`, `.annotation` on a local→`Local`/on a field→`Field`,
  `.call-result`→`CallResult`, `.alias`→`Alias`, `.pattern`→`Pattern`, `.return`→`Return`), enclosing
  scope via the existing `context_for`/`type_context_at`. Populate `SymbolDef.param_types`/`return_type`
  (methods/ctors) and `declared_type` (fields) from the formal-parameter / return / field-type nodes.
  Set `ReferenceSite.in_callable` from `CallableContext.id` (already collected in `java.rs`).
- Keep types raw (simple names like `OwnerService`, `List`) — *resolution* of raw→FQCN is Phase 4's
  job (via imports + same-package + same-file scopes). Re-emit `parsed-files.jsonl` keyed by the
  post-resolve version (see §4.5 — versioning now folds in the IR + resolved edges, so an IR-only
  change still bumps the version).

---

## Phase 4.1 — `cih-resolve`: build the resolution indexes (port `finalize`)

Read all `ParsedFile`s for the scope and build read-only, cross-file indexes (mirror
`finalize-orchestrator` + `workspace-index.ts`):

- **Def index** — `fqcn → SymbolDef` for every type; `(owner_fqcn, name) → [Method overloads]`;
  `(owner_fqcn, name) → Field`.
- **Type registry** — `simple_name → [fqcn]` (for raw→FQCN), and per-file an **import table**
  (`RawImport` → fqcn) + same-package siblings, so a raw type name resolves: explicit import →
  same-package → `java.lang.*` → unresolved.
- **Heritage adjacency** — from the `Extends`/`Implements` `ReferenceSite`s (resolved to FQCNs):
  `class → [superclass, interfaces]`, plus the reverse for interface→implementors.
- **Scope/type binding map** — per callable (`in_fqcn`), the `TypeBinding`s + field `declared_type`s,
  indexed by receiver name. Lookup is **precedence-ordered**, NOT a flat `(in_fqcn, name)` collapse
  (which can't tell a param from a local from a field, or handle shadowing/aliases): for a receiver in
  a callable resolve in order —
  1. nearest **`Param`/`Local`** binding by `kind` then range proximity to the use site (handles
     shadowing within the callable),
  2. **`Alias`/`CallResult`** chains (`var x = svc.get();` → return type of `get`),
  3. enclosing class **`Field`** `declared_type`,
  4. `this`/`super` → enclosing class / its MRO.
  Full block-level shadowing is bounded — range-proximity within the callable is the heuristic; deeper
  nested-block scoping is a later refinement.

The graph-id lookup cascade (port `ids.ts::resolveDefGraphId`): given an owner FQCN + member name +
arity, probe **qualified key (fqcn#name/arity)** → arity-only → simple name. Param-type fingerprint
for overloads is a later refinement (Phase-4 ids are arity-keyed today).

## Phase 4.2 — Resolve reference sites (port the emit passes, **order is load-bearing**)

Per `pipeline/run.ts`, with a per-site `handled` set so passes don't double-emit:

1. **`emit_receiver_bound_calls`** — the 7-case dispatcher (port `receiver-bound-calls.ts`), adapted
   to Java. For a `ReferenceSite{ kind:Call, receiver:Some(r), name, arity, in_fqcn }`:
   - **super branch** — `r == "super"` → resolve through the enclosing class's MRO chain.
   - **Case 0 (compound)** — `r` contains `.`/`(` → resolve the receiver expression's type first
     (chained `a.b().c()`), then the member on that type.
   - **Case 1 (namespace/static type)** — `r` is itself a known type/FQCN (`Foo.staticCall()`) →
     member on that type.
   - **Case 2 (field/var type)** — `r` is a field or local whose `declared_type` resolves to a class
     `C` → walk `C`'s MRO + `find_owned_member(name, arity)`. **This is the `service.findOwner()`
     path.**
   - **Case 3 (dotted/`this`/chain typebinding)** — `this.m()` → enclosing class; `var`-inferred
     locals via the call-result/alias bindings.
   - **Case 5 (value-receiver bridge)** — receiver is a known const/var but type unresolved → best
     effort, lower confidence, else drop.
   - On success emit `CALLS` (src = `site.in_callable` — the caller's `NodeId` persisted in 4.0;
     dst = resolved member id). Mark handled. **Do not** use the raw `in_fqcn` string as a src: it is
     `fqcn#name/arity`, not a graph id — emitting it would create dangling edges. If `in_callable`
     were ever absent, reconstruct via `cih_core::{method_id, constructor_id}` (`"Constructor:"` when
     the name segment is `<init>`, else `"Method:"` + `in_fqcn`).
2. **`emit_free_call_fallback`** — bare calls (`helper()`, no receiver) → lexical/inheritance chain
   of the enclosing class, then same-file/imported free functions.
3. **`emit_references_via_lookup`** — drain remaining unhandled refs (field read/write →
   `ACCESSES`/`USES`, type refs → `USES`) via the lookup cascade; skip `handled` sites.
4. **`emit_import_edges`** — `IMPORTS` from File → resolved imported type.
5. **Heritage** — `Extends`→`EXTENDS`, `Implements`→`IMPLEMENTS` (resolve the raw super/interface
   name to an FQCN via the import table).

Unresolved target → **drop the edge + bump a `skipped` counter** (same semantics as GitNexus
`references-to-edges.ts`); collect the unresolved *external* FQCNs (see 4.4).

## Phase 4.3 — MRO (port `mro-processor.ts`)

Over the heritage adjacency (CSR-style), C3-linearize each class across its single superclass +
interfaces (cached per class). For each method, walk the linearization: emit `METHOD_OVERRIDES`
(superclass method, same name/arity) and `METHOD_IMPLEMENTS` (interface method). MRO also backs the
super-branch and Case-2 member walks in 4.2.

## Phase 4.4 — (separable) demand-driven JAR API surface — NOT in the base milestone

This step wires Task 8 but is **not required** for the "accurate call graph" milestone (which is met
by resolving in-scope source). It also has a real prerequisite that doesn't exist yet: the scanner
emits `jars: Vec::new()` and `.jar` is hard-ignored, so there is no JAR catalog to extract from. Do it
as two explicit steps, after the core passes are working:

- **4.4a — JAR discovery (prerequisite).** Populate `RepoMap.jars` (`crates/cih-engine/src/scan.rs`):
  catalog dependency JARs from Maven/Gradle dep lists, `~/.m2`, `lib/`/`libs/`, and
  `.workspace-dependencies/`. This is **metadata only** — do NOT un-ignore `.jar` in
  `scan/ignore_rules.rs` for the tree-sitter source walk; JARs stay out of parsing and are just listed
  (path, group/artifact, own-vs-third-party via `own_group_ids`).
- **4.4b — Extraction.** Collect the **unresolved external FQCN set** from 4.2's dropped edges, feed
  it to `cih_jar::JarApiExtractor::with_include(set)` over the cataloged JARs, route the emitted
  signature nodes through the same `GraphArtifacts`/`bulk_load` path, then re-run the lookup so app→lib
  `CALLS` land on the lib's API node instead of dropping.

## Phase 4.5 — Confidence + edges + load (with corrected versioning)

- Edges carry `confidence: f32` by evidence (port `evidence-weights.ts` concept): exact
  type-binding + arity match = 1.0; arity-only / MRO-walk = ~0.8; value-receiver best-effort = ~0.5.
- **Versioning change (required).** Today `cih-engine` computes `content_version` over **structure
  nodes/edges only**, right after parse (`crates/cih-engine/src/main.rs`). That misses IR-only changes
  (a method-body edit that adds a call but doesn't shift node ranges) and the resolved edges entirely.
  Move versioning to **after resolve** and hash the *full* output: structure nodes/edges **+** resolved
  edges (`CALLS`/`ACCESSES`/`USES`/heritage/MRO) **+** a hash of the `ParsedFile` IR. New engine order:
  parse → resolve → **version+emit**. Key `.cih/parsed/<v>/` and `.cih/artifacts/<v>/` by that
  post-resolve version. Keep `content_version` inputs deterministically sorted so re-run stays
  idempotent.
- Map resolved edges → `GraphArtifacts` (Phase 2) → `bulk_load`. Resolution is **additive** to the
  Phase-3 structure graph under the (new) version/graph key; re-run idempotent (MERGE).
- Engine: run resolve right after parse in `analyze` (default), or expose `cih-engine resolve <repo>`
  that reads the persisted `parsed-files.jsonl` for the current scope version.

---

## Parallelism

Build the def/type/heritage indexes once (single pass), then resolve reference sites with `rayon`
(read-only shared indexes; per-site edges into thread-local buffers, merged + deduped after). This is
the structural win over GitNexus's single-threaded resolver, not just a constant factor.

## Verification (done when)

- Index `spring-petclinic`: `service.findOwner()` / `repo.save()` resolve to the right method ids;
  `@Autowired`-interface fields still resolve to the *declared interface* method (concrete-impl
  routing is Phase 13). MCP `impact("…save")` returns real callers; `call_chain` walks `CALLS`.
- `EXTENDS`/`IMPLEMENTS`/`METHOD_OVERRIDES` present and correct on a small hierarchy.
- Node/edge counts sane vs GitNexus on the same repo; unresolved-drop counter reported.
- Re-run idempotent; the **version changes when only a method body changes** (IR-only edit), proving
  the post-resolve versioning; workspace clippy clean; per-pass unit tests (one per receiver-bound
  case, mirror GitNexus `test/integration/resolvers/java.test.ts`).
- **(4.4 only, not the base milestone)** after 4.4a JAR discovery, a demand-driven run: an app call
  into a fixture lib lands on the `cih-jar` API node (ids byte-identical — already proven in Task 8).

## Risks / decisions

- **Type inference depth.** Field/param/local declared types cover most Spring calls; full expression
  typing (deep chains, generics erasure) is bounded — accept lower recall, grow the case corpus from
  the real repo. The receiver-bound case corpus is the moat, not Rust.
- **Overloads.** Arity-keyed ids now; add a param-type fingerprint to the id scheme only if overload
  collisions show up in practice (keep Phase-3 ids stable until then).
- **Raw→FQCN resolution** (imports, same-package, `java.lang`, wildcards) — unit-test the import
  table; wildcard imports are best-effort.
- **DI-aware resolution** (interface→`@Service` impl) is **Phase 13**, not here — Phase 4 resolves to
  the declared (often interface) type; Phase 13 rewrites the binding to the concrete bean.

## Ordered task checklist

1. **4.0** Extend `cih-core::ir`: `TypeBinding { kind: BindingKind, .. }`, `SymbolDef` type fields,
   and `ReferenceSite.in_callable: NodeId`. `cih-parse` persists type bindings (mapped to
   `BindingKind`), param/return/field types, and `in_callable` (from `CallableContext.id`); unit-test.
2. **4.1** `cih-resolve`: load `ParsedFile`s → def/type/heritage/import indexes + **precedence-ordered**
   scope-binding lookup (param/local → alias/call-result → field → this/super) + the id lookup cascade.
3. **4.2** Emit passes in order: receiver-bound (7 cases) → free-call → references-via-lookup →
   import edges → heritage; per-site dedup; **edge src = `site.in_callable`**; drop-unresolved counter.
4. **4.3** C3 MRO → `METHOD_OVERRIDES`/`METHOD_IMPLEMENTS`; back the super/Case-2 walks.
5. **4.5** Confidence weights; **version after resolve** over structure+resolved+IR; map →
   `GraphArtifacts` → `bulk_load`; engine order parse→resolve→version+emit (or `resolve` subcommand);
   verify on spring-petclinic (incl. version-bumps-on-body-change); mark ROADMAP.
6. **4.4 (separable, post-milestone)** — **4.4a** JAR discovery into `RepoMap.jars` (metadata only,
   keep `.jar` ignored for parsing); **4.4b** unresolved external FQCNs → `cih_jar::with_include` → load.
