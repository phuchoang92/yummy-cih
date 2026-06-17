# Plan: Split `cih-resolve/src/lib.rs` into Focused Modules

## Goal

Reduce `crates/cih-resolve/src/lib.rs` from 2,614 lines to a small facade by distributing
implementation into five focused modules. No runtime behavior changes. The crate's public API
and all 29 tests must pass unmodified in intent.

---

## Current state

```
crates/cih-resolve/src/
  lib.rs         2614 lines  — everything: types, index, emitter, helpers, tests
  reports.rs       96 lines  — unchanged output writers (already separate)
  db_access.rs    371 lines  — unchanged DB access detection (already separate)
```

Downstream callers (only `cih-engine/src/analyze.rs`):
```rust
cih_resolve::resolve_edges(&parse_output.parsed_files)
cih_resolve::emit_db_access(&parse_output.parsed_files)
cih_resolve::write_unresolved_reports(&resolve_output.unresolved_refs, &artifacts_dir)
```
None of these must change signature or behavior.

---

## Target state

```
crates/cih-resolve/src/
  lib.rs            ~60 lines  — module decls, public types, re-exports, resolve_edges
  types.rs         ~105 lines  — pure string/type helpers (no struct dependencies)
  index.rs         ~445 lines  — ResolveIndex, FileContext, build/lookup methods
  contracts.rs     ~110 lines  — resolve_contract_edges (standalone public fn)
  emit.rs          ~635 lines  — EdgeEmitter, emit passes, build_mro_map, c3_linearize
  tests.rs        ~1300 lines  — existing unit tests (moved from lib.rs)
  reports.rs        ~96 lines  — unchanged
  db_access.rs     ~371 lines  — unchanged
```

---

## Module responsibility map

### `lib.rs` (facade only)

Keeps:
- `mod` declarations for all new modules
- `pub use` re-exports preserving public API
- Public types `UnresolvedRef` and `ResolveOutput` (simple data structs; keeping them
  here avoids import-direction flip in `reports.rs` and `emit.rs`)
- `pub fn resolve_edges` (3-line orchestrator calling `ResolveIndex::build` then
  `EdgeEmitter::new(...).run()`)

### `types.rs` — pure helpers

All functions here take only standard-library or `cih_core` primitive types.
No dependency on `ResolveIndex` or any crate-local struct.

| Function | Visibility | Callers after split |
|---|---|---|
| `is_type_kind` | `pub(crate)` | `index.rs` |
| `simple_of` | `pub(crate)` | `index.rs`, `tests.rs` (test helper calls it) |
| `class_of` | `pub(crate)` | `index.rs`, `emit.rs` |
| `base_type_name` | `pub(crate)` | `index.rs` |
| `pick_binding` | `pub(crate)` | `index.rs` |
| `binding_rank` | private | `types.rs` only (called by `pick_binding`) |
| `is_simple_ident` | `pub(crate)` | `emit.rs` |
| `starts_uppercase` | `pub(crate)` | `emit.rs` |
| `call_name` | `pub(crate)` | `emit.rs` |
| `split_last_dot_outside_parens` | `pub(crate)` | `emit.rs` |
| `stable_dedup` | `pub(crate)` | `index.rs` (`ResolveIndex::dedup`) |

### `index.rs` — ResolveIndex

Moves:
- `struct ResolveIndex` (keep `pub(crate)`)
- `struct FileContext` (keep **private** within `index.rs` — `EdgeEmitter` never accesses its
  fields directly, only calls methods on `ResolveIndex`)
- `impl ResolveIndex` — methods that were already `pub(crate)` stay `pub(crate)`; private
  methods stay private. One exception: `di_impl` was private in `lib.rs` but must become
  `pub(crate)` because `emit.rs` calls `self.index.di_impl()` across the module boundary.

One new method required (see Cross-module visibility issue below):
```rust
pub(crate) fn all_methods(&self) -> &HashMap<(String, String), Vec<SymbolDef>> {
    &self.methods
}
```

Uses: `use crate::types::{is_type_kind, simple_of, class_of, base_type_name, pick_binding, stable_dedup};`

### `contracts.rs` — resolve_contract_edges

Moves:
- `pub fn resolve_contract_edges` (lines 1049–1149)

This function has no dependency on `ResolveIndex` or `EdgeEmitter`. It iterates
`pf.contract_sites` directly. Keeping it separate rather than in `emit.rs` avoids a
conceptual muddle (it is not part of the reference-resolution pipeline).

`lib.rs` adds `pub use contracts::resolve_contract_edges;` to preserve the public API.

Note: `contracts.rs` also needs `use std::collections::BTreeMap` for internal node
deduplication — this is not in `cih_core` and must be added explicitly.

### `emit.rs` — EdgeEmitter + MRO helpers

Moves:
- `struct EdgeEmitter` and `impl EdgeEmitter` (lines 493–1046)
- `fn build_mro_map` (line 1159) — stays with its **sole caller** `emit_mro_edges`
- `fn c3_linearize` (line 1171) — private helper for `build_mro_map`

`build_mro_map` and `c3_linearize` both take `&ResolveIndex` but only call
`pub(crate)` methods (`type_fqcns()`, `supertypes()`). No private field access needed,
so they can live here without additional visibility changes.

Uses:
```rust
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use cih_core::{file_id, Edge, EdgeKind, Node, NodeId, ParsedFile,
               RawImport, RefKind, ReferenceSite};
use crate::{UnresolvedRef, ResolveOutput};
use crate::index::ResolveIndex;
use crate::contracts::resolve_contract_edges;
use crate::types::{call_name, class_of, is_simple_ident,
                   split_last_dot_outside_parens, starts_uppercase};
```

Do **not** include `BindingKind` or `SymbolDef` — `BindingKind` is not used directly in
`emit.rs`, and `SymbolDef` is only accessed through type inference via `all_methods()` and
never named explicitly. Both would produce unused-import warnings.

### `tests.rs` — unit tests

Moves the entire `#[cfg(test)] mod tests { ... }` block (lines 1320–2614).

Replaces `use super::*;` with explicit imports:
```rust
use crate::index::ResolveIndex;
use crate::types::simple_of;
use crate::resolve_edges;
use cih_core::{
    constructor_id, external_endpoint_id, field_id, file_id, kafka_topic_id, method_id, type_id,
    BindingKind, ContractKind, ContractSite, EdgeKind, NodeId, NodeKind, Range, ReferenceSite,
    RawImport, SymbolDef, TypeBinding, ParsedFile,
};
```

Do **not** import `resolve_contract_edges`, `UnresolvedRef`, or `ResolveOutput` — the tests
never name those types explicitly (results are accessed via field names on the inferred return
value of `resolve_edges()`). Importing them would produce unused-import warnings. `file_id`
and `NodeId` are required by tests that construct node IDs directly.

All 23 test functions and their private helper functions (`type_def`, `method_def`,
`field_def`, `ctor_def`, `binding`, `import`, `heritage`, `make_di_scenario`, `workspace`)
move intact. No test logic changes.

---

## Cross-module visibility issues

### Issue 1 — `emit_mro_edges` reads `self.index.methods` directly (line 766)

`EdgeEmitter::emit_mro_edges` iterates the raw `methods` HashMap field:
```rust
self.index.methods.iter().flat_map(|((owner, name), overloads)| { ... })
```

After the split, `methods` is a private field of `ResolveIndex` in `index.rs`.
**Fix:** add the `all_methods()` accessor to `ResolveIndex` (shown above). Update
`emit_mro_edges` to call `self.index.all_methods().iter()`.

### Issue 2 — `tests.rs` calls `simple_of` directly (line 1333 in `type_def` helper)

The `type_def` test helper uses `simple_of(fqcn)` to build the `name` field of a
`SymbolDef`. **Fix:** mark `simple_of` as `pub(crate)` in `types.rs` (listed above).
This is the only private helper the tests call directly — all other test assertions go
through `resolve_edges()` (the public API).

### Issue 3 — `reports.rs` imports `use crate::UnresolvedRef`

`reports.rs` currently does `use crate::UnresolvedRef`. Since `UnresolvedRef` stays in
`lib.rs`, this import is **unchanged** after the split.

---

## Implementation steps

Run `cargo test -p cih-resolve` before starting. Expected: 29 tests pass (23 in
`lib.rs` inline tests + 6 in `db_access.rs`).

### Step 1 — Create `types.rs`

Cut two **non-contiguous** ranges from `lib.rs` into `src/types.rs`:

- Lines 1152–1157 (`fn stable_dedup`) — the one helper that precedes `build_mro_map`
- Lines 1223–1318 (`fn is_type_kind` through `fn split_last_dot_outside_parens`) — all
  eleven pure helpers

**Do not cut lines 1158–1222** (`fn build_mro_map` and `fn c3_linearize`). Those
functions take `&ResolveIndex` and belong in `emit.rs` (Step 4). Cutting the entire range
1152–1318 would incorrectly place them in `types.rs`.

Mark these `pub(crate)` (they are currently private module-level fns in `lib.rs`):
`is_type_kind`, `simple_of`, `class_of`, `base_type_name`, `pick_binding`,
`is_simple_ident`, `starts_uppercase`, `call_name`, `split_last_dot_outside_parens`,
`stable_dedup`. Leave `binding_rank` private.

Add to `lib.rs`:
```rust
mod types;
use types::*;   // keep lib.rs compiling until later steps
```

Verify: `cargo check -p cih-resolve`

### Step 2 — Create `index.rs`

Cut lines 63–491 from `lib.rs` (`pub(crate) struct ResolveIndex` through the closing
`}` of `impl ResolveIndex`). Create `src/index.rs`.

Add the `all_methods` accessor inside `impl ResolveIndex`:
```rust
pub(crate) fn all_methods(&self) -> &HashMap<(String, String), Vec<SymbolDef>> {
    &self.methods
}
```

At the top of `index.rs`:
```rust
use std::collections::{HashMap, HashSet};
use cih_core::{BindingKind, NodeId, NodeKind, ParsedFile, RawImport, RefKind, SymbolDef,
               TypeBinding};
use crate::types::{is_type_kind, simple_of, class_of, base_type_name, pick_binding,
                   stable_dedup};
```

Add to `lib.rs`:
```rust
mod index;
use index::ResolveIndex;
```

Verify: `cargo check -p cih-resolve`

### Step 3 — Create `contracts.rs`

Cut lines 1048–1149 from `lib.rs` (the `pub fn resolve_contract_edges` function).
Create `src/contracts.rs`.

Add required imports at the top of `contracts.rs`:
```rust
use std::collections::BTreeMap;
use cih_core::{external_endpoint_id, kafka_topic_id, ContractKind, Edge, EdgeKind, Node,
               NodeKind, ParsedFile};
```

Add to `lib.rs`:
```rust
mod contracts;
pub use contracts::resolve_contract_edges;
```

Remove the now-duplicate `pub fn resolve_contract_edges` from `lib.rs`.

Verify: `cargo check -p cih-resolve`

### Step 4 — Create `emit.rs`

Cut two ranges from `lib.rs` into `src/emit.rs`:

- Lines 493–1047 (`struct EdgeEmitter` through the closing `}` of `impl EdgeEmitter`)
- Lines 1158–1222 (`fn build_mro_map` and `fn c3_linearize`) — these were intentionally
  left in `lib.rs` by Step 1 and are cut here

Place `build_mro_map` and `c3_linearize` after the `impl EdgeEmitter` block in `emit.rs`.

Update `emit_mro_edges` to use the accessor:
```rust
// Before:
self.index.methods.iter().flat_map(...)
// After:
self.index.all_methods().iter().flat_map(...)
```

At the top of `emit.rs`:
```rust
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use cih_core::{file_id, Edge, EdgeKind, Node, NodeId, ParsedFile,
               RawImport, RefKind, ReferenceSite};
use crate::{UnresolvedRef, ResolveOutput};
use crate::index::ResolveIndex;
use crate::contracts::resolve_contract_edges;
use crate::types::{call_name, class_of, is_simple_ident,
                   split_last_dot_outside_parens, starts_uppercase};
```

Add to `lib.rs`:
```rust
mod emit;
use emit::EdgeEmitter;
```

Verify: `cargo check -p cih-resolve`

### Step 5 — Slim down `lib.rs`

At this point `lib.rs` should contain only:
- The module-level doc comment
- `mod types; mod index; mod contracts; mod emit;`
- `pub mod db_access; pub mod reports;`
- `pub use` re-exports
- The `use` statements needed for `UnresolvedRef`, `ResolveOutput`, and `resolve_edges`
- `pub struct UnresolvedRef { ... }`
- `pub struct ResolveOutput { ... }`
- `pub fn resolve_edges`

Remove the now-empty `use types::*;` glob and replace with only what `lib.rs` itself
uses (nothing — `resolve_edges` delegates entirely to `ResolveIndex` and
`EdgeEmitter`).

Target `lib.rs` skeleton:
```rust
//! Phase 4.1/4.2 — resolution indexes and reference-site edge emission.
// ... module doc ...

use cih_core::{Edge, Node, NodeId, ParsedFile, Range};
use serde::{Deserialize, Serialize};

pub mod db_access;
pub mod reports;
mod types;
mod index;
mod contracts;
mod emit;

pub use db_access::emit_db_access;
pub use reports::write_unresolved_reports;
pub use contracts::resolve_contract_edges;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UnresolvedRef { ... }   // keep definition here

#[derive(Clone, Debug, Default)]
pub struct ResolveOutput { ... }   // keep definition here

pub fn resolve_edges(parsed: &[ParsedFile]) -> ResolveOutput {
    let index = index::ResolveIndex::build(parsed);
    emit::EdgeEmitter::new(parsed, index).run()
}
```

Verify: `cargo check -p cih-resolve`

### Step 6 — Create `tests.rs`

Move the `#[cfg(test)] mod tests { use super::*; ... }` block (lines 1320–2614 of the
original `lib.rs`) to `src/tests.rs`.

Replace `use super::*;` with the explicit import list shown in the module section above.

In `lib.rs`, replace the inline `mod tests` block with:
```rust
#[cfg(test)]
mod tests;
```

Verify: `cargo test -p cih-resolve` — expect 29 tests, 0 failures.

### Step 7 — Final checks

```bash
cargo fmt --package cih-resolve
cargo test -p cih-resolve          # 29 tests
cargo test --workspace             # no regressions in cih-engine
cargo clippy -p cih-resolve --all-targets -- -D warnings
```

---

## Acceptance criteria

- [ ] `src/lib.rs` is ≤60 lines (module decls + public types + `resolve_edges`)
- [ ] `cargo test -p cih-resolve` passes with 29 tests
- [ ] `cargo test --workspace` passes (no downstream regressions)
- [ ] `rg "cih_resolve::"` — zero call-sites changed
- [ ] No `pub` items added beyond the new `all_methods()` accessor (`pub(crate)`)
- [ ] `db_access.rs` and `reports.rs` diff shows zero changes (or only import-path fixes)

---

## What does NOT change

- All resolver algorithms: receiver-binding precedence, MRO/C3 linearization, DI
  redirect logic, unresolved classification taxonomy, edge confidence values
- Edge kinds emitted (`Calls`, `Accesses`, `Uses`, `Imports`, `Extends`, `Implements`,
  `MethodOverrides`, `MethodImplements`, `ExternalCall`, `KafkaPublish`, `KafkaListens`)
- `UnresolvedRef` fields and `reason` taxonomy strings
- `ResolveOutput` fields
- Test assertions — every test moves verbatim, no logic changes

---

## Notes

- `build_mro_map` and `c3_linearize` live in `emit.rs` (not `index.rs` or `types.rs`)
  because they are only called from `EdgeEmitter::emit_mro_edges`. They need only
  `pub(crate)` methods on `ResolveIndex` (`type_fqcns()`, `supertypes()`), so no
  additional visibility is required.
- `resolve_contract_edges` lives in its own `contracts.rs` because it is an independent
  public entry point that shares no code with `EdgeEmitter`. Putting it in `emit.rs`
  would misrepresent the dependency relationship.
- The `#[cfg(test)] pub(crate) fn implementors` on `ResolveIndex` (line 429) moves
  intact to `index.rs` — the attribute already limits it to test builds.
- `fn di_impl` was private in the original `lib.rs` (called only from within that file).
  After the split it must become `pub(crate)` because `emit.rs` calls
  `self.index.di_impl()` across the module boundary. This is the only method whose
  visibility must be promoted during the split.
