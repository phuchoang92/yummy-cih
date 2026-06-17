# Plan: Pluggable Language Support

## Problem

Adding a second language (Python, Go, TypeScript) today requires touching **four places in core**:

1. `cih-engine/src/scan/java_scan.rs` — hardcoded `.ends_with(".java")` extension filter
2. `cih-parse/src/lib.rs:157` — hardcoded `java::parse_java_file(rel, &src)` dispatch
3. `ScanResult.java_files` — Java-named field
4. `cih-lang/src/lib.rs` — `pub mod java;` list

The `LanguageProvider` trait exists in `cih-lang` but is never used for dispatch — it's
orphaned. The fix wires it into the actual parse and scan paths.

---

## Root Cause: Circular Dependency

`LanguageProvider` is in `cih-lang`. `ParsedUnit` (the output of parsing one file) is in
`cih-parse`. If we add `parse_file` to `LanguageProvider`, `cih-lang` would need to import
`cih-parse`, which already imports `cih-lang` → circular dependency.

**Solution:** Move `ParsedUnit` and its IR sub-types to `cih-core`. They're already
language-agnostic (imports, refs, SQL sites, declarations as generic strings). They belong
in `cih-core` alongside `Node`/`Edge`.

---

## Target Architecture

```
cih-core    ← Node, Edge, NodeKind, ParsedUnit (IR moved here)
cih-lang    ← LanguageProvider trait (includes parse_file → ParsedUnit)
              JavaProvider implements LanguageProvider
              [future: PythonProvider, GoProvider, ...]
cih-parse   ← LanguageRegistry, parse_files(registry, ...)
              no longer knows about Java specifically
cih-engine  ← builds LanguageRegistry at startup
              passes registry to scan + parse
```

### Adding Python after this refactor

1. Create `crates/cih-lang/src/python/mod.rs` with `PythonProvider: LanguageProvider`
2. Add `tree-sitter-python` to `cih-lang/Cargo.toml`
3. One line in `cih-engine/src/analyze.rs`:
   ```rust
   r.register(cih_lang::python::PythonProvider::new());
   ```

**Zero changes** to `cih-parse`, `cih-core`, scan, or dispatch logic.

---

## Changes

### Step 1 — Move IR types to `cih-core`

Move from `cih-parse/src/ir.rs` → `cih-core/src/ir.rs`:
- `ParsedUnit`, `ParsedFile`, `FileRef`, `MethodRef`, `ContractSite`, `SqlConstant`, and
  any other IR structs

Re-export from `cih-parse` for backward compatibility (keeps `cih-resolve` untouched):
```rust
// cih-parse/src/lib.rs
pub use cih_core::ir::{ParsedUnit, ParsedFile, ...};
```

---

### Step 2 — Extend `LanguageProvider` trait and move Java parse logic

**In `cih-lang/src/lib.rs`**, add to the trait:
```rust
pub trait LanguageProvider: Send + Sync {
    fn language(&self) -> tree_sitter::Language;
    fn extensions(&self) -> &'static [&'static str];
    fn scope_query(&self) -> &tree_sitter::Query;
    fn package_of(&self, root: tree_sitter::Node<'_>, src: &str) -> Option<String>;
    fn stereotype(&self, def_text: &str) -> Option<Stereotype>;
    fn parse_file(&self, rel: &str, src: &str) -> anyhow::Result<cih_core::ir::ParsedUnit>; // NEW
}
```

**In `cih-lang/src/java/mod.rs`**, implement `parse_file`:
- Move the body of `parse_java_file` + all helpers (`collect_declarations`,
  `collect_spring_routes`, `collect_sql_constants`, etc.) from `cih-parse/src/java.rs`
  into `cih-lang/src/java/`

Add `cih-core` to `cih-lang/Cargo.toml`.

---

### Step 3 — Registry-based dispatch in `cih-parse`

**Add `LanguageRegistry`** to `cih-parse/src/lib.rs`:
```rust
pub struct LanguageRegistry {
    providers: Vec<Box<dyn LanguageProvider>>,
}
impl LanguageRegistry {
    pub fn new() -> Self { Self { providers: vec![] } }
    pub fn register(&mut self, p: impl LanguageProvider + 'static) {
        self.providers.push(Box::new(p));
    }
    pub fn provider_for(&self, path: &str) -> Option<&dyn LanguageProvider> {
        self.providers.iter()
            .find(|p| p.extensions().iter().any(|ext| path.ends_with(ext)))
            .map(|p| p.as_ref())
    }
    pub fn all_extensions(&self) -> Vec<&'static str> {
        self.providers.iter().flat_map(|p| p.extensions().iter().copied()).collect()
    }
}
```

**Change `parse_one`** to dispatch via registry:
```rust
fn parse_one(registry: &LanguageRegistry, repo_root: &Path, rel: &str) -> Result<ParsedUnit> {
    let src = fs::read_to_string(repo_root.join(rel))?;
    registry.provider_for(rel)
        .ok_or_else(|| anyhow::anyhow!("no language provider for {rel}"))?
        .parse_file(rel, &src)
}
```

**Delete** `cih-parse/src/java.rs` — logic is now in `cih-lang/src/java/`.

---

### Step 4 — Generalize scan in `cih-engine`

**In `cih-engine/src/scan/java_scan.rs`** (rename to `source_scan.rs`):
```rust
// Before
files.iter().filter(|f| f.path.ends_with(".java"))

// After
let exts = registry.all_extensions();
files.iter().filter(|f| exts.iter().any(|ext| f.path.ends_with(ext)))
```

Rename `ScanResult.java_files: Vec<OwnedJavaFile>` → `source_files: Vec<OwnedSourceFile>`.

---

### Step 5 — Registry builder in `cih-engine`

**In `cih-engine/src/analyze.rs`**:
```rust
fn default_registry() -> LanguageRegistry {
    let mut r = LanguageRegistry::new();
    r.register(cih_lang::java::JavaProvider::new());
    r
}
```

Pass `&registry` to `scan_repo(registry, &repo)` and `parse_files(registry, ...)`.

---

## What stays Java-specific (out of scope)

| Component | Reason |
|---|---|
| `cih-jar` (JAR/bytecode) | Explicitly Java; needs `cafebabe` + `.class` files |
| Build-system scan (Maven/Gradle) | Java-specific; Python would use pyproject.toml |
| `Stereotype` enum (Spring/JaxRs) | Lives in `cih-lang`, not core; Python provider returns `None` |

---

## Files changed

| File | Change |
|---|---|
| `crates/cih-core/src/ir.rs` (new) | IR types moved from `cih-parse/src/ir.rs` |
| `crates/cih-core/src/lib.rs` | Add `pub mod ir;` |
| `crates/cih-lang/src/lib.rs` | Add `parse_file` to trait |
| `crates/cih-lang/src/java/mod.rs` | Implement `parse_file`; absorb helpers |
| `crates/cih-lang/Cargo.toml` | Add `cih-core` dependency |
| `crates/cih-parse/src/lib.rs` | Add `LanguageRegistry`; dispatch via registry |
| `crates/cih-parse/src/java.rs` | **Delete** |
| `crates/cih-engine/src/scan/java_scan.rs` | Use `registry.all_extensions()` |
| `crates/cih-engine/src/scan.rs` | Rename `java_files` → `source_files` |
| `crates/cih-engine/src/analyze.rs` | `default_registry()`; pass to parse + scan |

---

## Acceptance criteria

- [ ] `cargo check --workspace` — clean compile
- [ ] `cargo test --workspace` — all existing tests pass
- [ ] Adding a mock second provider in `cih-parse` tests proves dispatch works for two extensions
- [ ] Adding `PythonProvider` stub in `cih-lang` + one-line registration in `analyze.rs`
      makes `.py` files picked up in scan — no other files touched
