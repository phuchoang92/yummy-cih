# Phase 3 — Engine MVP: scan → scope → parse → load (detailed plan)

Goal: turn a target ("des") repo into real graph data in FalkorDB — but **scan and let the
user scope first**, so a 12k-file repo doesn't force an all-or-nothing index. Output of Phase 3:
`cih-engine scan <repo>` prints a module map + recommendation; `cih-engine analyze <repo>
[--all | --module … | --scope …]` parses the chosen scope and loads it via the Phase-2 path.

Builds on: Phase 2 (`GraphArtifacts` + `FalkorStore::bulk_load`). Ports from GitNexus:
`languages/java/query.ts` (scope query), `core/ingestion/pipeline-phases/{scan,structure,parse}.ts`.

New crates: `cih-parse`, `cih-lang` (JavaProvider), `cih-engine` (the CLI/orchestrator).

---

## Why scan + scope first (the refinement)

Parsing everything up front is expensive and often unwanted: third-party/decompiled code,
generated sources, and unrelated modules bloat the graph and the first index. So Phase 3 splits
into a **fast, parse-free discovery pass** that summarizes the repo by module/folder, then asks
the user to **index all, or pick a subset to consume first**. Scope is persisted so re-indexes
are consistent, and unselected code simply becomes an out-of-scope boundary (calls into it
terminate, like external libs — same mechanism as un-decompiled deps).

---

## Phase 3.0 — Crate setup

- `cih-lang`: `LanguageProvider` trait + `JavaProvider`. Deps: `tree-sitter`, `tree-sitter-java`.
- `cih-parse`: parallel parse driver. Deps: `rayon`, `ignore` (gitignore-aware walk), `globset`.
- `cih-engine`: bin (`clap`) orchestrating scan/scope/parse/load; depends on `cih-parse`,
  `cih-lang`, `cih-graph-store`, `cih-falkor`, `cih-core`.
- Extend `cih-core` with `RepoMap`/`ModuleInfo` and the parse IR (below).

## Phase 3.1 — Repo scan & discovery (NO tree-sitter, must stay fast)

A filesystem + light-header pass only — target seconds even on 12k files.

1. **Walk** the repo with `ignore` (respects `.gitignore`); skip `target/`, `build/`, `.git/`,
   `node_modules/`, and binary files. Note `.workspace-dependencies/` (decompiled deps) separately.
2. **Detect build units → modules:**
   - **Maven:** every dir with `pom.xml` is a module; parent `<modules>` gives the tree.
   - **Gradle:** `settings.gradle[.kts]` `include`s / dirs with `build.gradle[.kts]`.
   - **Fallback:** top-level source roots / top-level Java packages as pseudo-modules.
3. **Per module, cheaply collect:** path, build file, `.java` file count, byte size + approx LOC
   (newline count — no full parse), top-level Java packages, and a **light Spring signal** via
   substring counts over `.java` (no AST): `@RestController/@Controller/@Service/@Repository/
   @Component/@Configuration/@Entity`, `@RequestMapping`/`@*Mapping`. Best-effort sibling-module
   deps from `pom.xml` `<dependency>` on other modules.
4. **Emit `RepoMap`** → `.cih/repo-map.json` + a printed summary (table/tree) and a
   **recommendation with a rough cost estimate** (files × ~parse-ms, est. nodes), e.g.:
   ```
   Module                    .java   LOC    svc  ctrl  repo  entity   est.nodes
   payments-core               820   91k     54    12    33      40      ~14k
   payments-api                310   28k      6    41     2       1       ~5k
   .workspace-dependencies/  26575  3.1M      –     –     –       –     ~430k  (decompiled)
   → Recommend: start with `payments-core` + `payments-api` (~19k nodes, ~30s),
     defer decompiled deps. Or `--all` (~3 min, big).
   ```

`cih-engine scan <repo> [--json]` performs 3.1 only.

## Phase 3.1b — JAR / own-lib catalog + decompile choice

The scan also catalogs **dependency JARs**, separating **your own libs** from third-party:
- Sources: Maven/Gradle dependency lists, the local repo (`~/.m2`), and `.jar` files under
  `lib/`, `libs/`, `.workspace-dependencies/`.
- **Own vs third-party** by groupId / package prefix (configurable, e.g.
  `own_group_ids = ["com.acme"]`). Own libs are worth tracing into; third-party (Spring, Apache,
  …) usually adds only noise.
- Summary lists own JARs separately, e.g. `8 internal JARs (com.acme.*) · 142 third-party (skipped)`.

**Per own-lib handling — three levels (default: skip).** Many "own" libs ship as JARs with **no
source** (e.g. binary-distributed internal libs), so "index from source" is NOT an option for them.
Choose per JAR / groupId:
1. **Skip** — opaque boundary; calls into it drop. Cheapest.
2. **API surface (recommended for source-less libs).** Read the JAR (a zip) → parse `.class`
   bytecode with a Rust class-file parser (`cafebabe` / `noak`) → emit **signature-only** nodes
   (Class/Interface/Method/Field with FQCN + descriptor-derived param/return types, **no bodies**),
   tagged `fromJar=true`. App→lib `CALLS`/`USES` then **resolve to a real target** instead of
   dropping → you get "what app code depends on this lib API" + "what breaks if this API changes",
   with **no decompiler, no JDK**, no method-body noise, no graph doubling.
3. **Full decompile** — Fernflower → `.workspace-dependencies/` → parse (**Phase 8**). Lets you
   trace lib *internals*; noisy (synthetic members, desugared lambdas) and ~doubles the graph.
   Reserve for the few libs whose internals you must trace *through*.

**Demand-driven (the 26k-lib unlock).** Don't API-extract all libs — after app parsing, the
*unresolved-reference set* names exactly which external classes the app touches; API-extract **only
those**, collapsing 26k JARs to just-what's-used. `--lib-api all` for full completeness if ever needed.
Detection + the recorded per-lib choice live here; full-decompile execution is Phase 8 (scope
persists the decision).

## Phase 3.1c — Optional LLM enrichment (`--describe`, off by default)

The structural scan stays the deterministic source of truth. With `--describe`, after the
structural pass an LLM adds a **one-line description per module** and an **assisted "what to consume
first" suggestion** — fed only cheap signals (module name, top packages, top class names, README
excerpt, Spring counts), **not full source**. Blurbs are cached in `repo-map.json` (keyed by content
hash) so they aren't re-run. A deterministic heuristic recommendation (app modules first; defer
generated / decompiled / third-party) is always present; the LLM only enriches it. For banking
compliance, route via the in-account Claude path (e.g. Bedrock) and prefer metadata over raw code.

## Phase 3.2 — Scope selection

- User picks via:
  - CLI: `--all`, `--module name1,name2`, `--include <glob>`, `--exclude <glob>`,
    `--include-decompiled`.
  - or a persisted **`cih.scope.toml`**:
    ```toml
    include = ["payments-core", "com.acme.payments.*"]
    exclude = ["**/generated/**"]
    include_decompiled = false
    ```
- Resolve selection against the `RepoMap` → a concrete **file list** to parse. Persist the
  effective scope to `.cih/scope.json` so re-index is reproducible. Default (no flags): print the
  summary and ask the user to choose (don't silently index everything).
- Out-of-scope calls are left unresolved at Phase 4 (boundary), not errors.
- **Own-lib JARs:** per JAR/groupId choose **skip / API-surface (recommended) / full-decompile**.
  Default skip; API-surface is demand-driven (only app-referenced classes). Persisted to
  `.cih/scope.json`; full-decompile executed by Phase 8. (Source-less libs → API-surface, not source.)

## Phase 3.3 — Parse the selected scope (tree-sitter Java)

- `JavaProvider` (cih-lang): lazy `Parser`/`Query` singletons; the **scope query** ported from
  `languages/java/query.ts` (`@scope.*`, `@declaration.*`, `@import.statement`, type bindings,
  `@reference.*`).
- `cih-parse`: `rayon` parallel over the scoped file list. Per file, walk captures →
  - **Defs:** Class/Interface/Enum/Record/Annotation, Method/Constructor, Field — each with its
    **FQCN** (package decl + nested-class chain), name, range, modifiers.
  - **Structure edges:** `File`/`Folder` nodes + `CONTAINS`; class→member `HAS_METHOD`/`HAS_FIELD`;
    nested-class `CONTAINS`.
  - **Collect (don't resolve yet):** raw imports + `ReferenceSite`s (call/field/heritage sites)
    into the IR — **Phase 4** resolves these into `CALLS`/`EXTENDS`/… edges.
- Phase 3 emits **structure only**; the call graph arrives in Phase 4.

## Phase 3.4 — Emit + load

- Map defs/edges → `Node`/`Edge` and write `GraphArtifacts` → `FalkorStore::bulk_load` (Phase 2).
- **Node-id scheme (LOCK IT NOW — Phase 4 resolution depends on it):**
  - `File:<repo-rel-path>`, `Folder:<repo-rel-path>`
  - `Class|Interface|Enum|Record|Annotation:<fqcn>` (e.g. `Class:com.acme.UserService`)
  - `Method:<fqcn>#<name>/<arity>` (constructor: `Constructor:<fqcn>#<init>/<arity>`) — arity now;
    add param-type fingerprint in Phase 4 for overloads.
  - `Field:<fqcn>#<name>`
  - FQCN-based (not path-based) so ids are stable across the graph DB; flag duplicate-FQCN
    collisions (decompiled dups) for later.

## Phase 3.5 — `cih-engine` CLI

```
cih-engine scan   <repo> [--json]                         # 3.1 → repo-map + summary + recommendation
cih-engine analyze <repo> [--all | --module a,b | --scope cih.scope.toml]
                          [--graph-key cih] [--falkor-url …]   # 3.2–3.4
```
Env mirrors the server: `FALKOR_URL` (default `redis://127.0.0.1:6380`), `CIH_GRAPH_KEY`.

---

## Data model additions (`cih-core`)

```rust
struct RepoMap { root, scanned_at, build_system, total_files, total_loc,
                 languages: Map<String,u64>, modules: Vec<ModuleInfo>, decompiled_dirs: Vec<String> }
struct ModuleInfo { name, rel_path, build_file: Option<String>, java_files: u64, loc: u64,
                    packages: Vec<String>, spring: SpringSignal, depends_on: Vec<String> }
struct SpringSignal { controllers, services, repositories, components, configs, entities, mappings: u32 }
// parse IR (structure now; references consumed in Phase 4)
struct ParsedFile { file, package: Option<String>, defs: Vec<SymbolDef>,
                    imports: Vec<RawImport>, reference_sites: Vec<ReferenceSite> }
struct SymbolDef { id: NodeId, kind: NodeKind, fqcn: String, name: String, owner: Option<NodeId>,
                   range: Range, modifiers: Vec<String> }
```

## Verification (Done when)

- `cih-engine scan <spring-petclinic>` → correct modules/packages, ~N `.java`, sensible
  service/controller/entity counts, and a recommendation line.
- `cih-engine analyze <spring-petclinic> --all` → classes + methods in FalkorDB; MCP
  `context("…OwnerController")` lists its methods; node count ≈ classes + methods + fields.
- Scoped run (`--module …`) indexes only that module; out-of-scope symbols absent.
- Re-run is idempotent (Phase-2 MERGE) and respects persisted `.cih/scope.json`.

## Risks / decisions

- **`tree-sitter-java` version** + query parity with `languages/java/query.ts` — port & test incrementally.
- **FQCN derivation** (package decl, nested classes, same simple names) — unit-test it.
- **ID scheme stability** — locked in 3.4; Phase 4 keys off it.
- **Scan must stay light** — approx LOC via newline count, no full AST; summarize decompiled dirs
  but default-exclude from `--all` unless `--include-decompiled`.
- **LLM enrichment** (3.1c) — keep it opt-in (`--describe`), cached, metadata-only, compliance-routed;
  the deterministic scan + heuristic recommendation stay the default and source of truth.
- **Own/third-party JAR split** depends on a configurable groupId/package prefix; get it wrong and
  you decompile noise or skip real internal libs. Make `own_group_ids` explicit in config.

## Ordered task checklist

1. `cih-core`: `RepoMap`/`ModuleInfo`/`SpringSignal` + parse IR + node-id helpers.
2. `cih-engine scan`: walk (`ignore`) + module detection + light Spring counts → `repo-map.json` + summary + recommendation.
3. Scope resolution: CLI flags + `cih.scope.toml` → effective file list → `.cih/scope.json`.
4. `cih-lang` JavaProvider: parser/query singletons + Java scope query.
5. `cih-parse`: rayon parse → defs + structure edges + collected refs (IR).
6. Emit `Node`/`Edge` (locked id scheme) → `GraphArtifacts` → `bulk_load`.
7. `cih-engine analyze`: wire 3.2–3.4; verify on spring-petclinic; mark roadmap.
8. JAR catalog: detect dependency JARs, split own vs third-party by `own_group_ids` (3.1b).
9. Scope: per-own-lib decompile decision → persist to `.cih/scope.json` (executed by Phase 8).
10. Optional `--describe`: LLM module blurbs + assisted suggestion (cached, metadata-only, compliance-routed).
11. **JAR API-surface extraction** (Rust `cafebabe`/`noak`): read `.class` → signature-only nodes
    (`fromJar=true`), **demand-driven** from Phase 4's unresolved-reference set. The high-value path
    for the 26k source-less own libs (no JDK, no decompiler). Full-decompile stays the rare exception (Phase 8).
