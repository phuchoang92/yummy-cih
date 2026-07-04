# Phase 3 — Implementation Spec (handoff to an implementing AI)

This is the **build instruction** for Phase 3. Read `phase-3.md` first for intent; this doc gives
concrete types, algorithms, acceptance criteria, and a **port map** of what to copy from GitNexus
(`/Users/phuc/BigMoves/AI/GitNexus/gitnexus`) vs. write fresh. Implement tasks in order; each task
is independently testable.

**Prereqs (already done):** Phase 1 (MCP server, `cih-server`), Phase 2 (`GraphArtifacts` JSONL +
`FalkorStore::bulk_load`/`upsert_incremental`). Dev: FalkorDB on `:6380`,
`FALKOR_URL=redis://127.0.0.1:6380` (a Homebrew redis squats 6379).

---

## 0. Invariants (do not violate)

- **Node-id scheme (LOCKED — Phase 4 resolution keys off this):**
  - `File:<repo-rel-path>`, `Folder:<repo-rel-path>`
  - `Class|Interface|Enum|Record|Annotation:<fqcn>` — e.g. `Class:com.acme.user.UserService`
  - `Method:<fqcn>#<name>/<arity>`; `Constructor:<fqcn>#<init>/<arity>`
  - `Field:<fqcn>#<name>`
  - FQCN = `package` + nested-class chain + simple name, dot-separated. Nested classes use `.`
    (normalize bytecode `$` → `.`).
- **All writes go through Phase 2** (`GraphArtifacts::write` → `FalkorStore::bulk_load`). Do not
  add a second write path.
- **Phase 3 emits STRUCTURE only** (File/Folder/Class/Method/Field + CONTAINS/HAS_METHOD/HAS_FIELD).
  Call/heritage edges are Phase 4 — but Phase 3 MUST collect `ReferenceSite`s + imports into the IR
  so Phase 4 can resolve them.
- Reuse `cih-core`, `cih-graph-store`, `cih-falkor` as-is; add new crates `cih-lang`, `cih-parse`,
  `cih-engine`, `cih-jar`.

## GitNexus port map (what to copy)

| Area | GitNexus source | Action |
|---|---|---|
| File walk (scan paths+sizes, no content) | `src/core/ingestion/filesystem-walker.ts` (`walkRepositoryPaths`) | PORT → Rust `ignore` crate |
| Ignore list + extensions + .gitignore | `src/config/ignore-service.ts` (`DEFAULT_IGNORE_LIST`, `IGNORED_EXTENSIONS`) | PORT (copy the lists; `.jar/.class/.war` already ignored) |
| File/Folder nodes + CONTAINS + id scheme | `src/core/ingestion/structure-processor.ts`; `src/lib/utils.ts` (`generateId`) | PORT (1:1) |
| Java tree-sitter query | `src/core/ingestion/languages/java/query.ts` (full `JAVA_SCOPE_QUERY`) | COPY the `.scm` verbatim |
| FQCN / qualified name | `class-extractors/generic.ts` (`buildQualifiedName`), `languages/java/package-siblings.ts` (`extractPackageName`) | PORT the algorithm |
| Language provider pattern | `src/core/ingestion/language-provider.ts` | MIRROR as a trimmed Rust trait |
| Spring stereotype + route detection | `framework-detection.ts` (`detectFrameworkFromAST`), `route-extractors/spring.ts` (`extractSpringRoutes`) | PORT (Task 7) |
| Maven/Gradle parse | `core/group/extractors/java-workspace-extractor.ts` (`parsePom`, `parseGradle`) | PORT the regex parsers |
| Module **grouping into a tree** | — (GitNexus is flat) | NET-NEW |
| JAR bytecode → API surface | — (none; `.jar` ignored) | NET-NEW (Task 8) |

---

## Crate layout & dependencies

```
cih-lang/   LanguageProvider trait + JavaProvider     deps: tree-sitter, tree-sitter-java
cih-parse/  rayon parse driver → IR + structure       deps: cih-lang, cih-core, rayon
cih-jar/    JAR → API-surface signature nodes         deps: cih-core, zip, cafebabe
cih-engine/ clap CLI: scan / analyze                  deps: all of the above + cih-falkor, ignore, quick-xml, toml, clap
cih-core/   + RepoMap/ModuleInfo + parse IR + id helpers (extend existing)
```

---

## Task 1 — `cih-core`: types + id helpers

Add `repo_map.rs` and `ir.rs` modules + node-id helpers. Concrete Rust:

```rust
// --- repo map (Task 2 output) ---
#[derive(Serialize, Deserialize)]
pub struct RepoMap {
    pub root: String,
    pub build_system: BuildSystem,          // Maven | Gradle | None
    pub total_java_files: u64,
    pub total_loc: u64,
    pub modules: Vec<ModuleInfo>,
    pub jars: Vec<JarInfo>,                  // Task 2/8
    pub decompiled_dirs: Vec<String>,
}
#[derive(Serialize, Deserialize)]
pub struct ModuleInfo {
    pub name: String, pub rel_path: String,
    pub build_file: Option<String>,
    pub java_files: u64, pub loc: u64,
    pub packages: Vec<String>,              // top-level java packages
    pub spring: SpringSignal,
    pub depends_on: Vec<String>,            // sibling module names (best-effort from pom/gradle)
}
#[derive(Default, Serialize, Deserialize)]
pub struct SpringSignal { pub controllers:u32, pub services:u32, pub repositories:u32,
                          pub components:u32, pub configs:u32, pub entities:u32, pub mappings:u32 }
#[derive(Serialize, Deserialize)]
pub struct JarInfo { pub path:String, pub group_id:Option<String>, pub artifact:Option<String>,
                     pub is_own:bool, pub classes:u64 }

// --- parse IR (Task 5 output; references consumed in Phase 4) ---
pub struct ParsedFile {
    pub file: String, pub package: Option<String>,
    pub defs: Vec<SymbolDef>,
    pub imports: Vec<RawImport>,
    pub reference_sites: Vec<ReferenceSite>,   // unresolved; Phase 4 resolves
}
pub struct SymbolDef { pub id: NodeId, pub kind: NodeKind, pub fqcn: String, pub name: String,
                       pub owner: Option<NodeId>, pub range: Range, pub modifiers: Vec<String> }
pub struct RawImport { pub raw: String, pub is_static: bool, pub is_wildcard: bool, pub range: Range }
pub struct ReferenceSite { pub name:String, pub receiver:Option<String>, pub kind:RefKind,
                           pub arity:Option<u16>, pub range:Range, pub in_fqcn:String }
pub enum RefKind { Call, FieldRead, FieldWrite, Ctor, Extends, Implements, TypeRef }

// --- id helpers (LOCKED scheme) ---
pub fn file_id(rel: &str) -> NodeId            { NodeId::new(format!("File:{rel}")) }
pub fn folder_id(rel: &str) -> NodeId          { NodeId::new(format!("Folder:{rel}")) }
pub fn type_id(kind: NodeKind, fqcn: &str) -> NodeId  // Class:/Interface:/...
pub fn method_id(fqcn:&str, name:&str, arity:u16)->NodeId  // Method:fqcn#name/arity
pub fn field_id(fqcn:&str, name:&str)->NodeId
```
**Acceptance:** crate compiles; round-trip `RepoMap`/`ParsedFile` through serde_json; id helpers
unit-tested (incl. nested-class FQCN `com.acme.Outer.Inner`).

## Task 2 — `cih-engine scan` (discovery, NO tree-sitter)

**2a. Walk** (PORT `filesystem-walker.ts` + `ignore-service.ts`): use the Rust `ignore` crate
(honors `.gitignore` + `.cihignore`). Hardcode the ported `DEFAULT_IGNORE_LIST` (`target build out
bin obj .git node_modules vendor generated ...`) and skip `IGNORED_EXTENSIONS` (`.jar .class .war
.ear` + binaries). Collect `.java` paths + byte size; LOC = newline count (no full parse — keep it
fast). Note `.workspace-dependencies/` separately.

**2b. Module detection** (PORT `parsePom`/`parseGradle` from `java-workspace-extractor.ts`; the
**tree grouping is net-new**): every dir with `pom.xml` (or `build.gradle[.kts]`) is a module; parse
`<groupId>/<artifactId>` + `<dependency>` (Maven via `quick-xml`, Gradle via regex). Maven parent
`<modules>` → child tree; Gradle `settings.gradle` `include` → modules. Fallback: top-level source
roots. Map sibling deps (`groupId:artifactId` pointing at another module) → `ModuleInfo.depends_on`.

**2c. Light Spring signal** (ADAPT `detectFrameworkFromAST` to a cheap substring scan — NO AST):
per module, count `.java` files containing `@RestController`/`@Controller`/`@Service`/`@Repository`/
`@Component`/`@Configuration`/`@Entity` and `@*Mapping`. Plain string search, parallel via rayon.

**2d. Output:** write `.cih/repo-map.json`; print a module table + a **deterministic recommendation**
(app modules first; defer generated/decompiled/third-party) + a rough cost estimate
(`files × ~parse-ms`, est. nodes). `cih-engine scan <repo> [--json]`.

**Acceptance:** `cih-engine scan <spring-petclinic>` lists the module(s), correct `.java` count,
non-zero services/controllers/entities, and a recommendation line. Runs in seconds.

## Task 3 — Scope selection

`analyze` flags: `--all`, `--module a,b`, `--include <glob>`, `--exclude <glob>`,
`--include-decompiled`; or `cih.scope.toml` (`toml` crate). **Default (no flag): print the scan
summary and exit asking the user to choose — never auto-index.** Resolve selection against
`RepoMap` → concrete file list; persist effective scope to `.cih/scope.json`.
**Acceptance:** `--module x` indexes only x's files; default prints summary + exits non-zero with a
"choose a scope" message.

## Task 4 — `cih-lang`: JavaProvider + scope query

- COPY `JAVA_SCOPE_QUERY` verbatim from `languages/java/query.ts` into `cih-lang/src/java/query.scm`
  (include it via `include_str!`). Lazy `tree_sitter::Parser` + `Query` singletons (`once_cell`).
- Trimmed Rust trait (Phase-4 hooks added later):
```rust
pub trait LanguageProvider: Send + Sync {
    fn language(&self) -> tree_sitter::Language;
    fn extensions(&self) -> &'static [&'static str];
    fn scope_query(&self) -> &tree_sitter::Query;
    fn package_of(&self, root: tree_sitter::Node, src: &str) -> Option<String>; // PORT extractPackageName
    fn stereotype(&self, def_text: &str) -> Option<Stereotype>;                 // PORT detectFrameworkFromAST
}
pub struct JavaProvider { /* parser+query singletons */ }
```
**Acceptance:** parse a Java string; query matches `@declaration.class`/`@declaration.method`/
`@reference.call.*`; `package_of` returns `com.example` for a packaged file.

## Task 5 — `cih-parse`: parse → structure + IR

- `rayon::par_iter` over the scoped file list. Per file: parse → run scope query → build `ParsedFile`.
- **FQCN** (PORT `buildQualifiedName` + `extractPackageName`): package decl + walk class-declaration
  ancestors for nested chain + simple name. Methods/fields owned by their enclosing type.
- **Emit structure:** File/Folder + CONTAINS (PORT `structure-processor.ts`); Class/Interface/Enum/
  Record/Method/Constructor/Field nodes (locked ids); `HAS_METHOD`/`HAS_FIELD` from type→member;
  nested-type `CONTAINS`.
- **Collect (don't resolve):** `RawImport`s from `@import.statement`; `ReferenceSite`s from
  `@reference.call.*`/`@reference.read.*`/heritage captures — store with `in_fqcn` (enclosing
  callable) for Phase 4.
- Return `(Vec<Node>, Vec<Edge>, Vec<ParsedFile>)`. The `ParsedFile`s persist (e.g. `.cih/parsed/`)
  for Phase 4.
**Acceptance:** parse spring-petclinic → class/method/field counts match a manual count on a sample
file; nested-class FQCNs correct.

## Task 6 — Emit + load (reuse Phase 2)

Map nodes/edges → `GraphArtifacts::write(.cih/artifacts, version, &nodes, &edges)` →
`FalkorStore::bulk_load`. `version` = content hash of the scope+inputs.
**Acceptance:** after `analyze --all`, FalkorDB has the classes/methods; MCP `context("…OwnerController")`
lists its methods; re-run is idempotent.

## Task 7 — Spring tags (PORT framework-detection + spring routes)

PORT `detectFrameworkFromAST` (set `props.stereotype` on class nodes) and `extractSpringRoutes`
(emit `Route` nodes + `HANDLES_ROUTE` edges, class-level `@RequestMapping` prefix + method mapping).
**Acceptance:** controllers tagged; a `@GetMapping("/owners")` yields a `Route` node linked to its handler.

## Task 8 — `cih-jar`: API-surface extraction (NET-NEW; central to the 26k source-less libs)

No decompiler, no JDK. Read JAR (zip) → parse each `.class` with the **`cafebabe`** crate → emit
signature-only nodes. Algorithm:

1. **Open JAR** with `zip` crate; iterate entries ending `.class` (skip `module-info`, `package-info`).
2. **Parse class** with `cafebabe::parse_class(bytes)` → class name, access flags, fields, methods.
3. **Internal-name → FQCN:** replace `/`→`.`, `$`→`.` (nested). Skip synthetic/anonymous
   (`Name$1`, names with digits-only segments) unless `--lib-api all`.
4. **Emit nodes (tag `props.fromJar=true`, `props.external=true`):**
   - `Class`/`Interface`/`Enum`/`Annotation:<fqcn>` (from access flags)
   - `Method:<fqcn>#<name>/<arity>` (arity from descriptor param count — see mapping); `<init>`→Constructor
   - `Field:<fqcn>#<name>`
   - `HAS_METHOD`/`HAS_FIELD` edges. (No bodies, no CALLS — it's the API surface.)
5. **JVM descriptor → type mapping** (for param/return display + Phase-4 overload keys):
   `B`→byte `C`→char `D`→double `F`→float `I`→int `J`→long `S`→short `Z`→boolean `V`→void
   `L<name>;`→class (`/`→`.`) `[X`→`X[]`. Method desc `(params)ret`: count top-level params for arity.
6. **Demand-driven (default):** accept a set of referenced FQCNs (from Phase 4's unresolved refs)
   and only emit classes in that set. `--lib-api all` emits the whole JAR.
7. Feed emitted nodes/edges through the **same** `GraphArtifacts`/`bulk_load` path.

```rust
pub struct JarApiExtractor { pub include: Option<HashSet<String>> /* referenced FQCNs */ }
impl JarApiExtractor {
    pub fn extract(&self, jar_path: &Path) -> anyhow::Result<(Vec<Node>, Vec<Edge>)>;
}
```
**Acceptance:** point at a small JAR (e.g. a tiny internal lib); emitted `Method` ids match what the
app side resolves to, so an app→lib `CALLS` (added in Phase 4) lands on the JAR's method node rather
than dropping. Verify ids are byte-identical between app-resolved target and JAR-emitted node.

---

## End-to-end verification

1. `cih-engine scan <spring-petclinic>` → sane module/Spring summary + recommendation.
2. `cih-engine analyze <spring-petclinic> --all` → structure in FalkorDB; MCP `context` on a
   controller lists its methods; node count ≈ classes+methods+fields.
3. `--module <one>` indexes only that module.
4. (Task 8) extract a sample JAR's API → ids align with app-side FQCN ids.
5. Re-run idempotent (Phase-2 MERGE); `.cih/scope.json` respected.

## Risks / notes

- `tree-sitter-java` grammar version must match the query's node types (`program`, `class_declaration`,
  `method_invocation`, `field_access`, `scoped_identifier`). Pin and test.
- FQCN edge cases: nested/anonymous classes, same simple names, default package — unit-test.
- `cafebabe` maturity: if a class fails to parse, skip + count (don't abort the JAR).
- Module-tree grouping is net-new; keep it best-effort (fallback to flat if no build files).
- Keep `scan` light: LOC via newline count, Spring via substring — NO tree-sitter in scan.
