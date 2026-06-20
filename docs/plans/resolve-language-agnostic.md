# Plan: Language-Aware `cih-resolve` Redesign

## Summary

Refactor `cih-resolve` around a language-partitioned index and per-language resolution
strategies. Moving Spring helpers into a Java folder is not sufficient: imports, symbol
qualification, receiver typing, constructors, and inheritance also vary by language.

V1 supports:

- Java with unchanged graph behavior and stable node IDs.
- TypeScript/TSX and Python cross-file resolution.
- Unknown languages with structural parsing only.
- No implicit cross-language symbol matching.
- Compatibility with the existing `resolve_edges()` API and legacy parsed artifacts.

## Normalized IR and Cache Compatibility

Add structured import data while retaining `RawImport` for compatibility:

```rust
pub struct ImportBinding {
    pub module: String,
    pub imported: Option<String>,
    pub local: Option<String>,
    pub kind: ImportBindingKind,
    pub range: Range,
}

pub enum ImportBindingKind {
    Named,
    Default,
    Namespace,
    Module,
    StaticMember,
    Wildcard,
}
```

Add `ParsedFile::import_bindings: Vec<ImportBinding>` with `#[serde(default)]`. Java,
TypeScript, and Python providers populate this normalized representation. Existing
`ParsedFile::imports` remains available to old consumers and serialized artifacts.

Additional compatibility changes:

- Add `language` to unresolved-reference diagnostics.
- Add a typed unresolved-external record containing `language` and `qualified_name`.
- Retain `ResolveOutput::unresolved_external_fqcns` as the Java-only compatibility view used
  by JAR extraction.
- When `ParsedFile::language` is empty, infer it from the file extension. Unknown extensions
  select the no-op resolver.
- Version parse-cache paths, for example `.cih/parse-cache/v2/`, so cached IR from the old
  schema is reparsed instead of silently reused.

## Language-Partitioned Resolution Core

### Index

Replace the global Java-shaped index with a language-aware `CommonIndex`:

- Key definitions by `(language, qualified_name)`.
- Key simple-name candidates by `(language, simple_name)`.
- Key members by `(language, container, member_name)`.
- Store language and normalized imports in each file context.
- Index `Function` definitions alongside methods and constructors so top-level
  TypeScript/Python calls can resolve.
- Never use workspace-unique simple-name fallback across languages.
- Allow cross-language links only through explicit contracts, external endpoints, events,
  database entities, or future configured bridges.

Rename Java-specific common terminology where the representation is actually shared. For
example, replace `class_of()` with `container_of_callable()`. Keep Java-only package,
wildcard-import, static-import, and overload rules inside the Java strategy.

### Resolver strategy

Create `lang/mod.rs` with `LanguageResolver`, `ResolverRegistry`, and `NoOpResolver`. Expose a
read-only `ResolveContext` facade to strategies instead of exposing `CommonIndex` directly.

Each resolver is responsible for:

- Resolving normalized imports and local aliases.
- Resolving raw type or symbol names in a file/module.
- Resolving special receivers such as `this`, `super`, `self`, and `cls`.
- Resolving constructor calls.
- Redirecting framework-managed receivers, such as Spring interface injection.
- Selecting inheritance semantics.
- Emitting language/framework-specific extra nodes and edges.

Use an explicit inheritance model rather than `uses_mro: bool`:

```rust
pub enum InheritanceModel {
    None,
    JavaLegacy,
    PythonC3,
    TypeScriptNominal,
}
```

The common emitter asks the resolver associated with each definition before emitting
inheritance, override, or implementation edges. It never runs one whole-graph MRO algorithm
over mixed-language definitions.

## Language Implementations

### Java

- Preserve current package, explicit/wildcard/static import, overload, constructor,
  receiver, DI, and hierarchy behavior.
- Preserve current edge IDs, confidence values, and reason strings.
- Move Spring DI, DI XML, and integration XML under `lang/java/`.
- Re-export the current XML extraction functions as compatibility wrappers.
- Run Java extras only when Java files are present and XML integration is enabled.
- Preserve the existing `--skip-xml-integration` behavior.

### TypeScript and TSX

- Resolve relative workspace imports and named, default, and namespace aliases.
- Resolve top-level functions, classes, explicit constructors, declared inheritance, and
  calls whose receiver type is known.
- Link named imports to symbols and namespace/module imports to file nodes.
- Treat package imports and unresolved `tsconfig` path aliases as external unless they map
  unambiguously to an indexed workspace module.
- Add parser bindings for typed parameters and typed local variables needed by receiver
  resolution.

### Python

- Resolve absolute and relative workspace imports, `from` imports, aliases, top-level
  functions, classes, explicit `__init__`, and declared inheritance.
- Resolve `self` and `cls` to the enclosing class.
- Use available type annotations for receiver bindings.
- Leave dynamic imports, monkey patching, and runtime-only duck typing unresolved rather than
  guessing.

### Shared emitters

Contract and SQL emitters remain common because they consume normalized parser IR. Java JAR
extraction receives only Java unresolved externals; TypeScript and Python external module
names must not be interpreted as Java FQCNs.

## Public API and Engine Wiring

Preserve the current entrypoint:

```rust
pub fn resolve_edges(parsed: &[ParsedFile]) -> ResolveOutput
```

It delegates to the default Java/TypeScript/Python registry.

Add the configurable entrypoint:

```rust
pub fn resolve_with_registry(
    parsed: &[ParsedFile],
    registry: &ResolverRegistry,
    options: ResolveOptions<'_>,
) -> ResolveOutput
```

`ResolveOptions` carries the optional repository root and `enable_xml_integrations` flag.

Engine wiring must:

- Build parser and resolver registries through one engine-level language-support catalog.
- Reject duplicate language IDs and validate that every parser language has an intended
  resolver.
- Group files by normalized language before invoking extra passes.
- Never pass every parsed file to every language resolver.
- Invoke extra passes in deterministic language-ID order.
- Append Java XML output at the same point in artifact assembly as today.
- Keep database, contract, and JAR stages in their current relative order.

Proposed resolver layout:

```text
crates/cih-resolve/src/
  lib.rs
  common/
    mod.rs
    index.rs
    emit.rs
    inheritance.rs
  lang/
    mod.rs
    java/
      mod.rs
      di.rs
      di_xml.rs
      integration_xml.rs
    typescript/
      mod.rs
      imports.rs
    python/
      mod.rs
      imports.rs
  contracts.rs
  db_access.rs
  reports.rs
  types.rs
```

Go is not part of this change. Adding it later requires a parser/provider, grammar and scan
support, a resolver strategy, engine catalog registration, and language-specific tests.

## Verification

Before refactoring, capture canonical sorted Java `ResolveOutput` fixtures. After each stage,
compare nodes, edges, confidence, reason strings, and unresolved diagnostics against that
baseline.

Required tests:

- Java package/import, overload, constructor, `this`/`super`, DI, inheritance, XML, and JAR
  behavior remains unchanged.
- Legacy parsed files with an empty language select the resolver inferred from their extension.
- Parse-cache schema changes force reparsing.
- TypeScript named/default/namespace imports, aliases, top-level calls, constructors,
  inheritance, and typed receivers resolve correctly.
- Python module/from imports, aliases, relative imports, top-level calls, `self`/`cls`,
  `__init__`, and multiple inheritance resolve correctly.
- A mixed-language fixture with identical symbol names emits no cross-language call,
  inheritance, import, or type-reference edges.
- Unknown languages produce structural parser edges without resolver failures or guessed
  references.
- XML extras run only for Java and respect the skip option.
- Registry validation rejects duplicate IDs and missing parser/resolver pairings.

Final checks:

```bash
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets
```

Run `analyze --all --no-load` against representative Java, TypeScript, Python, and mixed
fixtures, then compare the generated artifacts with their checked expectations.

## Assumptions and Boundaries

- V1 supports Java, TypeScript/TSX, and Python; Go is deferred.
- Java public APIs, node IDs, and graph semantics remain backward compatible.
- TypeScript package resolution and Python dynamic behavior are best-effort and never produce
  guessed edges.
- Cross-language application calls are represented through explicit contracts, not
  simple-name matching.
- MCP and wiki contracts do not change; they consume the improved graph.
