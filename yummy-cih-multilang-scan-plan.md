# Registry-Driven Multi-Language Scan Parity

## Summary

- Replace the Java-special-cased scan path with a registry-driven source scan for the languages already supported end-to-end today: Java, TypeScript, and Python.
- Make `repo-map.json` explicitly source-wide and accept the schema break now instead of carrying Java-named compatibility fields.
- Extend module discovery beyond Maven/Gradle so Node and Python repos get first-class module ownership, dependency mapping, reporting, and scope behavior.
- Keep the scope of this change to scan/module discovery/scope/report alignment. Parse and resolve already register Java, TypeScript, and Python; they should be wired to the same shared registry builder, not redesigned.

## Key changes

### Shared language registry and scan metadata

- Move the current `default_registry()` out of `analyze.rs` into a shared engine module so scan and analyze use the same registered providers and cannot drift.
- Keep `scan_repo(repo)` as the public entrypoint, but back it with `scan_repo_with_registry(repo, registry)` for internal reuse and tests.
- Add `SourceScan` to the language-provider surface with concrete fields:
  - `loc: u64`
  - `package: Option<String>`
  - `frameworks: BTreeSet<String>`
- Add `scan_file(rel: &str, src: &str) -> anyhow::Result<SourceScan>` to `cih_lang::LanguageProvider`.
- Keep framework detection per-file inside each provider and aggregate at module/repo level with set union. Normalize framework names to this fixed set in v1: `spring`, `nestjs`, `flask`, `fastapi`.
- Replace `JavaFileInfo`, `collect_java_files()`, and `collect_extra_source_files()` with a single `SourceFileInfo` collection path driven by the shared registry.
- `SourceFileInfo` should carry:
  - `path: String`
  - `language: String`
  - `loc: u64`
  - `package: Option<String>`
  - `frameworks: BTreeSet<String>`
- Add `language: String` to `OwnedSourceFile` so `scope.rs` stops re-deriving language from file extensions.

### Repo-map and aggregation model

Schema break — no legacy aliases and no dual-written Java-named fields:

| Old field | New field | Notes |
|---|---|---|
| `RepoMap.total_java_files` | removed | replaced by existing `total_source_files` |
| `RepoMap.total_loc` | `RepoMap.total_source_loc` | source-wide LOC |
| `ModuleInfo.java_files` | `ModuleInfo.source_files` | source-wide file count |
| `ModuleInfo.loc` | `ModuleInfo.source_loc` | source-wide LOC |
| `ModuleInfo.spring` | removed | replaced by generic `frameworks` |

Additional model changes:

- Keep `RepoMap.total_source_files` as the only repo-wide file-count field.
- Keep `RepoMap.per_language` as `BTreeMap<String, u64>`.
- Add `ModuleInfo.per_language: BTreeMap<String, u64>`.
- Keep `ModuleInfo.frameworks: Vec<String>`, stored sorted and unique. Aggregate by collecting the `BTreeSet<String>` fields from each `SourceFileInfo` via set union, then convert to `Vec` at module/repo boundary. Do not use `HashSet` at any aggregation step; the `BTreeSet` union guarantees sorted, deduplicated output without a separate sort pass.
- Keep `ModuleInfo.depends_on: Vec<String>`, but resolve sibling dependencies only within the same ecosystem.
- Keep `ModuleInfo.packages`, but redefine it as a best-effort namespace list. In v1, Java remains the only language required to populate it; TypeScript and Python may leave it empty unless a cheap namespace extraction already exists.
- Update `auto_detect_architecture()` to read `total_source_files` only; remove the fallback to deleted Java-only fields.
- Remove `SpringSignal` from `repo_map.json`, from `cih-core` exports, and from scan/report tests and docs that depend on it.

### Module detection and dependency mapping

Generalize module candidate discovery from Maven/Gradle-only to direct manifest scanning:

- `pom.xml`
- `build.gradle`, `build.gradle.kts`, `settings.gradle`, `settings.gradle.kts`
- `package.json`
- `pyproject.toml`
- `setup.cfg`
- `setup.py`
- `requirements.txt`

Rules:

- Rename the internal Java-specific artifact identity concept to a generic `module_key`.
- Maven and Gradle keep their current parsing behavior and dependency mapping, just under the renamed abstraction.
- Node:
  - every `package.json` forms a module candidate rooted at its directory
  - module name/key = `package.json.name` when present, otherwise directory basename
  - parse dependency sections from `dependencies`, `devDependencies`, `peerDependencies`, and `optionalDependencies`
  - resolve `depends_on` only when a dependency name matches another Node module candidate in the same repo
- Python:
  - `pyproject.toml` and `setup.cfg` form module candidates rooted at their directory
  - module name/key = normalized project/package name from metadata when present, otherwise directory basename
  - parse dependencies from `[project]`, Poetry sections, and equivalent `setup.cfg` metadata when present
  - resolve `depends_on` only when a dependency name matches another Python module candidate in the same repo
- `setup.py`:
  - create a module candidate only when the same directory has no `pyproject.toml` or `setup.cfg`
  - module name/key = directory basename
  - no dependency extraction
  - never execute code
- `requirements.txt`:
  - create a module candidate only when the same directory has no `pyproject.toml`, `setup.cfg`, or `setup.py`
  - module name/key = directory basename
  - no dependency extraction
- Cross-ecosystem dependency resolution is out of scope. A Java module does not depend on a Python module through `depends_on`; `BuildSystem::Mixed` is a repo-level summary only.
- Keep the root fallback/unassigned-owner behavior, but apply it to all scanned source files, not only Java.

### Reporting and downstream alignment

- Replace the Spring-centric scan table with generic columns: module, source files, source LOC, languages, frameworks, estimated nodes.
- Update recommendation scoring to prioritize application frameworks first (`spring`, `nestjs`, `flask`, `fastapi`) and then `source_files`.
- Keep one shared `PARSE_MS_PER_FILE` and `EST_NODES_PER_FILE` estimate across all supported languages in v1; do not introduce per-language weighting in this change.
- Update `analyze.rs` logging to use source-wide totals.
- Update `scope.rs` language filtering to use `OwnedSourceFile.language` instead of extension heuristics.
- Update docs that currently describe scan and `repo-map.json` as Java-only.

## Public APIs and schema changes

### `cih_lang::LanguageProvider`

- New method: `scan_file(rel: &str, src: &str) -> anyhow::Result<SourceScan>`

### `OwnedSourceFile`

- New field: `language: String`

### `repo-map.json` breaking changes

- Remove `total_java_files`
- Rename `total_loc` → `total_source_loc`
- Rename module `java_files` → `source_files`
- Rename module `loc` → `source_loc`
- Remove module `spring`
- Add module `per_language: BTreeMap<String, u64>`
- `ModuleInfo.packages` semantics change from “Java packages” to “best-effort namespace list”

### `SourceFileInfo` (replaces `JavaFileInfo`)

New struct replacing `JavaFileInfo` in the scan collection path:

- `path: String`
- `language: String`
- `loc: u64`
- `package: Option<String>`
- `frameworks: BTreeSet<String>`

Remove `JavaFileInfo` from all public exports and internal usages. `collect_java_files()` and `collect_extra_source_files()` are deleted; callers use the new registry-driven collection path returning `Vec<SourceFileInfo>`.

### `cih-core` public surface

- Remove `SpringSignal` from the exported `repo_map` model and `cih_core` re-exports.
- `BuildSystem` enum values are unchanged; only detection behavior broadens for existing `Node`, `Python`, and `Mixed` cases.

CLI flags are unchanged; the meaning of scan output becomes source-wide across Java, TypeScript, and Python.

## Test plan

### Provider-level unit tests for `scan_file`

- Java Spring file
- TypeScript NestJS file
- Python Flask file
- Python FastAPI file
- non-framework files for each language
- framework normalization is deterministic and sorted

### Scan integration tests

- Mixed Java/TS/Python repo — correct repo/module counts, LOC, `per_language`, frameworks, ownership
- Node-only repo with `package.json` module detection and sibling dependency mapping
- Python-only repo with `pyproject.toml` module detection and sibling dependency mapping
- Python `requirements.txt`-only project — deterministic module naming fallback, no sibling dependency mapping
- `setup.py`-only project — deterministic module naming fallback, no code execution
- Mixed-ecosystem repo producing correct `BuildSystem::Mixed`
- Unassigned non-Java files creating the root fallback module
- Repo with no recognized manifests (bare source tree) — root fallback still owns all files
- Cross-ecosystem dependencies are not emitted into `depends_on`
- `setup.py` is not executed — explicit negative test; no subprocess spawned

### Scope/analyze tests

- `--languages` filters by stored file language, not extension heuristics
- Scan and analyze share the same provider registry builder
- Java-only JAR/XML/DI steps still skip when selected scope has no Java files
- `auto_detect_architecture()` uses `total_source_files` after the schema change

### JSON round-trip tests

- New `RepoMap` schema round-trips
- New `ModuleInfo` schema round-trips
- No legacy Java-only fields or `SpringSignal` remain in serialized scan output

## Assumptions and constraints

- Parity is only for languages already registered end-to-end today: Java, TypeScript, Python.
- The schema break is intentional; no compatibility aliases are kept.
- Module discovery is manifest-based and non-executing; `setup.py` is never executed and arbitrary build scripts are never evaluated.
- JAR discovery and XML/DI enrichment remain Java-specific subsystems. Parity here means scan/scope/analyze consistency for supported languages, not identical enrichment behavior across all languages.
- No Django, Rails, Go, or other language support is included in this plan.
