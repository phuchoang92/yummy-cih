# Plan: Phase 16 — Test Intelligence

## Goal

Complete the **Tester persona** in yummy-cih. Add test detection to the parse layer, emit
`TESTS` edges in the graph, and expose three MCP tools: `test_coverage`, `regression_scope`,
and `untested_paths`.

No new external dependencies. Pure extension of the existing pipeline.

---

## What changes

### 1. `cih-core/src/lib.rs` — add `EdgeKind::Tests`

Add one variant to the existing `EdgeKind` enum:

```rust
pub enum EdgeKind {
    // … existing variants …
    Tests,   // test method → production method/class it covers
}
```

Add the Cypher label in `cypher_label()`:

```rust
EdgeKind::Tests => "TESTS",
```

---

### 2. `cih-parse/src/java.rs` — test class detection + `TESTS` edge emission

**`class_stereotype()` extension** — add test stereotypes to the existing match:

```rust
Some("SpringBootTest") | Some("ExtendWith") | Some("RunWith") => "test",
```

Plus naming-convention fallback in the call site (after annotation check):
```rust
// fallback: *Test / *Tests / *IT / *Spec naming
if simple_name.ends_with("Test") || simple_name.ends_with("Tests")
    || simple_name.ends_with("IT") || simple_name.ends_with("Spec")
{
    stereotype = Some("test");
}
```

**`TESTS` edge emission from `@MockBean` / `@Autowired` in test classes** — in
`collect_fields()`, when the enclosing class has `stereotype="test"`, emit a `TESTS` edge
from the test class node to the field's type (unresolved name → raw edge; Phase 4 resolve
will not touch it since it goes directly to a type node, not a method). This is a
**structural link**: "this test class exercises this production type."

**Method-level `@Test` annotation** — in `collect_method()`, detect if the method has a
`@Test` annotation. When found:
- Set `props["isTest"] = true` on the Method node.
- Emit a `TESTS` edge from the test method's `NodeId` to the **owner class** NodeId. This
  is the minimum reliable link (method→class); deeper call-target resolution happens at
  query time via existing `CALLS` edges.

No new IR types needed — `TESTS` edges go straight into `builder.edges`.

---

### 3. `cih-graph-store/src/lib.rs` — three new `GraphStore` trait methods

```rust
/// Return all test methods that have a direct TESTS edge to `symbol_id`,
/// or whose owner class does.
async fn test_coverage(&self, id: &NodeId) -> Result<Vec<Node>>;

/// Given a list of repo-relative changed file paths, return all test class
/// node ids that have a TESTS edge to any symbol in those files.
async fn tests_for_files(&self, files: &[String]) -> Result<Vec<Node>>;

/// Return all symbols in files under `file_prefix` that have NO inbound
/// TESTS edge (neither the symbol itself nor its owner class).
async fn untested_symbols(&self, file_prefix: &str, limit: usize) -> Result<Vec<Node>>;
```

---

### 4. `cih-falkor/src/lib.rs` — Cypher implementations

**`test_coverage(id)`**:
```cypher
MATCH (t:Symbol)-[:TESTS]->(target:Symbol)
WHERE target.id = $id
   OR (target.id IN [
         x IN [(c:Symbol)-[:HAS_METHOD]->(m:Symbol) WHERE m.id = $id | c.id] | x
       ])
RETURN t.id, t.kind, t.name, t.qualifiedName, t.file
ORDER BY t.file, t.name
LIMIT 50
```

Simplified version: two queries — one for direct hit, one for owner class — merged in Rust.

**`tests_for_files(files)`**:
```cypher
MATCH (t:Symbol)-[:TESTS]->(prod:Symbol)
WHERE prod.file IN $files
RETURN DISTINCT t.id, t.kind, t.name, t.qualifiedName, t.file
ORDER BY t.file, t.name
```

Plus: expand through `CALLS` edges one hop to catch test methods that call into the
changed files (avoids missing indirect coverage). Implementation: two Cypher queries, union
in Rust.

**`untested_symbols(file_prefix, limit)`**:
```cypher
MATCH (n:Symbol)
WHERE n.file STARTS WITH $prefix
  AND n.kind IN ['Method', 'Class', 'Interface']
  AND NOT (n.stereotype = 'test')
  AND NOT EXISTS { MATCH (:Symbol)-[:TESTS]->(n) }
  AND NOT EXISTS { MATCH (:Symbol)-[:TESTS]->(:Symbol)-[:HAS_METHOD]->(n) }
RETURN n.id, n.kind, n.name, n.qualifiedName, n.file
ORDER BY n.file, n.name
LIMIT $limit
```

---

### 5. `cih-server/src/main.rs` — three new MCP tools

**`test_coverage({ name })`**:
- Arg: `name: String` (symbol id or short name, same as `context`/`impact`)
- Calls `resolve_symbol()` → `store.test_coverage(id)`
- Returns: `{ symbol_id, test_count, tests: [{id, name, file}] }`

**`regression_scope({ changed_files, repo? })`**:
- Arg: `changed_files: Vec<String>` (repo-relative paths)
- Calls `store.tests_for_files(files)` → dedup by file → returns grouped by test file
- Returns: `{ changed_file_count, test_class_count, test_classes: [{id, name, file}] }`

**`untested_paths({ module_prefix, limit? })`**:
- Arg: `module_prefix: String` (e.g. `"src/main/java/com/acme/payment"`)
- Calls `store.untested_symbols(prefix, limit.unwrap_or(50))`
- Returns: `{ prefix, untested_count, symbols: [{id, kind, name, file}] }`

Update `get_info()` instructions to document the three new tools.

---

## Files to modify

| File | Change |
|---|---|
| `crates/cih-core/src/lib.rs` | Add `EdgeKind::Tests` + `"TESTS"` cypher label |
| `crates/cih-parse/src/java.rs` | Test stereotype detection; `@Test` method prop; `TESTS` edge emission from test fields |
| `crates/cih-graph-store/src/lib.rs` | Add `test_coverage`, `tests_for_files`, `untested_symbols` to `GraphStore` trait |
| `crates/cih-falkor/src/lib.rs` | Cypher implementations of the three new methods |
| `crates/cih-server/src/main.rs` | `test_coverage`, `regression_scope`, `untested_paths` MCP tools + `get_info()` update |

---

## Test plan

### Unit tests in `crates/cih-parse/src/`

Follow the existing parse test pattern (inline Java source, assert on emitted nodes/edges):

1. **Test class by annotation** — class with `@SpringBootTest` → `stereotype="test"` on the
   Class node's SymbolDef.
2. **Test class by naming convention** — class named `OrderServiceTest` (no annotations) →
   `stereotype="test"`.
3. **`@Test` method emits `TESTS` edge** — test class with a `@Test` method → Method node
   has `props["isTest"]=true` + `TESTS` edge from that method to the owner class.
4. **`@MockBean` field emits `TESTS` edge** — test class with `@MockBean OrderService svc`
   → `TESTS` edge from the test class node to the raw type name `"OrderService"` (unresolved;
   type name stored as `dst.id = "Class:com.acme.OrderService"` only after resolution, so
   the raw edge points to whatever `type_id(Class, fqcn)` resolves to if the class is in scope,
   otherwise skipped at emit time).

### Unit tests in `crates/cih-server/src/`

5. **Arg defaults** — `regression_scope` with no `limit`, `untested_paths` with no `limit` →
   serde defaults applied correctly.

### Engine integration test (`crates/cih-engine/src/tests.rs`)

Extend `temp_repo()` to add an `OwnerServiceTest.java` with `@SpringBootTest` +
`@MockBean OwnerService service` + one `@Test` method. Assert:
- The test class node has `stereotype="test"` in `nodes.jsonl`.
- `TESTS` edges exist in `edges.jsonl`.

### Run

```bash
~/.cargo/bin/cargo test -p cih-core
~/.cargo/bin/cargo test -p cih-parse
~/.cargo/bin/cargo test -p cih-server
~/.cargo/bin/cargo test -p cih-engine
~/.cargo/bin/cargo test --workspace
```

Expected: **≥ 124 tests green** (118 current + ~6 new).

---

## Implementation order

1. `EdgeKind::Tests` in `cih-core` (2 lines — needed by everything else)
2. Test detection in `cih-parse` (stereotype, `@Test` method prop, `TESTS` edge)
3. Parse unit tests (tests 1–4)
4. `GraphStore` trait methods in `cih-graph-store`
5. Cypher impls in `cih-falkor`
6. MCP tools in `cih-server` + `get_info()` update
7. Server arg-default tests (test 5)
8. Engine integration test
9. `cargo test --workspace` green
10. Update ROADMAP Phase 16 ✅

---

## Key design decisions

- **`TESTS` edge from test METHOD to production CLASS** (not method-to-method) is the
  reliable baseline. Method-to-method would require resolving `@MockBean` injection chains
  at parse time — too expensive. The three-hop query `test_method → owner_class → CALLS →
  prod_method` gives exact coverage at query time using already-indexed `CALLS` edges.

- **Naming convention as stereotype fallback** keeps the detection zero-annotation-dependency
  — many older Spring projects don't use `@SpringBootTest` but still follow `*Test` naming.

- **`untested_symbols` excludes `stereotype="test"` nodes** so test classes themselves don't
  appear in the gap report.

- **No new IR types** — `TESTS` goes straight into `builder.edges` in `cih-parse`, just like
  `CONTAINS`/`HAS_METHOD` edges. This keeps `ParsedFile`/`SymbolDef` stable.
