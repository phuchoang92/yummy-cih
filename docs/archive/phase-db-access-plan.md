# Plan: Table-Level DB Access Intelligence for Banking SQL Adapters

## Context

Banking code uses custom datasource adapters with static SQL constants and calls like
`DBUtil.prepareStatement(conn, QUERY_...)` / `DBUtil.executeQuery(...)` — not Spring Data
repositories. CIH should add table-level SQL access detection so it can answer which methods
read/write tables such as `CUSTOM_OVERDRAFT`, `CUSTOM_OVERDRAFT_EXTINFO`, and
`CUSTOM_EXTERNAL_OVERDRAFT`.

The Phase 10a wiki can immediately surface this in all three role pages:
- **PO**: "this module touches these core tables"
- **BA**: "workflow reads/writes these tables"
- **Dev**: method-level SQL constants, read/write table map, dynamic SQL warnings

---

## What changes

### `crates/cih-core/src/lib.rs`

Add to `NodeKind` enum:
```rust
DbQuery,   // a SQL statement (constant or inline)
DbTable,   // a normalized table name
```

Update `NodeKind::label()`, `NodeKind::from_label()`, and the round-trip test.

Add to `EdgeKind` enum:
```rust
ExecutesQuery,  // Method -> DbQuery
ReadsTable,     // DbQuery -> DbTable
WritesTable,    // DbQuery -> DbTable
```

Update `EdgeKind::cypher_label()`.

Add ID helper functions:
```rust
pub fn db_query_const_id(owner_fqcn: &str, const_name: &str) -> NodeId
pub fn db_query_inline_id(file: &str, line: u32, col: u32) -> NodeId
pub fn db_table_id(table: &str) -> NodeId   // table is normalized UPPERCASE
```

### `crates/cih-core/src/ir.rs`

Add new IR structs:
```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlConstant {
    pub const_name: String,   // field name, e.g. "QUERY_GETCUSTOMOVERDRAFTTYPEBYCODE"
    pub owner_fqcn: String,   // declaring class FQCN
    pub sql_text: String,     // folded SQL string (concatenated literals)
    pub dynamic: bool,        // true if any non-literal expression was in the init
    pub range: Range,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlExecutionSite {
    pub api_name: String,          // "executeQuery", "prepareStatement", etc.
    pub const_ref: Option<String>, // field name arg, e.g. "QUERY_GETCUSTOMOVERDRAFTTYPEBYCODE"
    pub inline_sql: Option<String>,// if SQL literal passed directly as argument
    pub in_callable: NodeId,
    pub range: Range,
}
```

Add to `ParsedFile`:
```rust
#[serde(default)]
pub sql_constants: Vec<SqlConstant>,
#[serde(default)]
pub sql_execution_sites: Vec<SqlExecutionSite>,
```

Export from `cih-core/src/lib.rs`:
```rust
pub use ir::{..., SqlConstant, SqlExecutionSite};
```

### `crates/cih-parse/src/sql.rs` (new)

Lightweight, Oracle-aware SQL table scanner — pure fn, no tree-sitter.

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableOp { Read, Write }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableAccess { pub table: String, pub op: TableOp }

pub fn scan_tables(sql: &str) -> Vec<TableAccess>
```

Algorithm:
1. Strip `/* ... */` block comments (including Oracle hints `/*+ ... */`)
2. Strip `-- ...` line comments
3. Uppercase the text
4. State-machine token scan by whitespace + `(`, `)`, `,`, `;`
5. After `FROM` or `JOIN` / `INNER JOIN` / `LEFT JOIN` / `RIGHT JOIN` / `CROSS JOIN`:
   - read next non-keyword token as table name
   - continue reading comma-separated tokens as additional tables (comma-join style)
6. After `INSERT INTO` or `MERGE INTO`: next token = table, op = Write
7. After `UPDATE`: next token = table, op = Write (skip `UPDATE OR IGNORE` etc.)
8. After `DELETE FROM`: next token = table, op = Write
9. Strip schema prefix: `SCHEMA.TABLE` → `TABLE`
10. Skip: `DUAL`, SQL keywords, anything containing `(` (subquery intro or function)
11. Tables are deduplicated per-op; if a table appears as both Read and Write, keep both

**Tests (inline in `sql.rs`):**
- `SELECT ... FROM A JOIN B` → reads A, B
- `SELECT ... FROM A a, B b WHERE` → reads A, B (aliases stripped)
- `INSERT INTO A (...)` → writes A
- `UPDATE A SET ...` → writes A
- `DELETE FROM A WHERE` → writes A
- `MERGE INTO A USING B` → writes A, reads B
- `SELECT ... FROM (SELECT ... FROM B) t` → reads B (subquery), skips `t`
- Oracle hint `SELECT /*+ INDEX(t IDX) */ * FROM A` → reads A
- Schema-qualified `FROM SCHEMA.A` → reads A
- `DUAL` skipped
- Dynamic SQL with no real table → empty result

### `crates/cih-parse/src/java.rs`

Add `collect_sql_constants` and `collect_sql_execution_sites` to `FileBuilder`.

**`collect_sql_constants`**: tree-sitter walk over `field_declaration` nodes:
- Has `static` + `final` modifiers
- Type node text = `String`
- Name: any SCREAMING_SNAKE_CASE identifier (let SQL scanner filter relevance)
- Init: fold string literals: `"A" + "B" + " C"` → `"A B C"`, concatenation with non-literals → `dynamic=true`, best-effort partial text
- Emits `SqlConstant { const_name, owner_fqcn (from nearest type_context), sql_text, dynamic, range }`

**`collect_sql_execution_sites`**: tree-sitter walk over `method_invocation` nodes:
- **DBUtil pattern**: receiver text = `DBUtil` (static call), method in `{prepareStatement, executeQuery, executeUpdate}`, 2nd argument (index 1) = identifier → `const_ref`
- **JdbcTemplate pattern**: receiver type matches `JdbcTemplate` (via `receiver_has_type`), method in `{query, update, queryForObject, queryForList, queryForMap}`, 1st argument = identifier or string literal → `const_ref` or `inline_sql`
- Emits `SqlExecutionSite { api_name, const_ref, inline_sql, in_callable, range }`

Add both calls to `parse_java_file`:
```rust
collect_sql_constants(root, src, &mut builder);
collect_sql_execution_sites(root, src, &mut builder);
```

Add fields to `FileBuilder`:
```rust
sql_constants: Vec<SqlConstant>,
sql_execution_sites: Vec<SqlExecutionSite>,
```

Populate `ParsedUnit`'s `parsed_file`:
```rust
parsed_file: ParsedFile {
    ...
    sql_constants: builder.sql_constants,
    sql_execution_sites: builder.sql_execution_sites,
}
```

### `crates/cih-resolve/src/db_access.rs` (new)

```rust
use cih_core::{
    db_query_const_id, db_query_inline_id, db_table_id,
    Edge, EdgeKind, Node, NodeKind, ParsedFile, SqlConstant,
};
use crate::sql_scanner::scan_tables;  // re-export from cih-parse or duplicate

pub fn emit_db_access(parsed: &[ParsedFile]) -> (Vec<Node>, Vec<Edge>)
```

Algorithm:
1. For each file, build `const_map: HashMap<(owner_fqcn, const_name), &SqlConstant>` from `sql_constants`
2. For each `sql_execution_site` in each file:
   a. Resolve SQL text:
      - `const_ref` present: look up `(owner_fqcn_of_in_callable, const_ref)` in `const_map`
      - `inline_sql` present: use directly (dynamic=false)
      - Neither: skip this site
   b. Run `scan_tables(sql_text)` → `table_accesses`
   c. If `table_accesses` is empty and not dynamic: skip (not a SQL constant)
   d. Build `DbQuery` node:
      - ID: `db_query_const_id(owner_fqcn, const_name)` or `db_query_inline_id(file, line, col)`
      - `name`: const_name or `"inline-sql"`
      - `props`: `{ "operation": primary_op, "constantName": ..., "sqlPreview": first_50_chars, "dynamic": bool, "tables": [...], "dialect": "oracle-like" }`
   e. Emit `EXECUTES_QUERY` edge: `in_callable → DbQuery`
   f. For each table access: emit / deduplicate `DbTable` node + `READS_TABLE` or `WRITES_TABLE` edge
3. Deduplicate nodes by id (same `DbTable` can be referenced by many queries)

Note: `owner_fqcn_of_in_callable` — derive from method id `Method:<fqcn>#<name>/<arity>` by
splitting at `#` and taking the left part.

### `crates/cih-engine/src/analyze.rs`

After the resolve phase, call `emit_db_access`:
```rust
let (db_nodes, db_edges) = cih_resolve::emit_db_access(&parse_output.parsed_files);
// merge into all_nodes and edges (existing merge point)
all_nodes.extend(db_nodes);
edges.extend(db_edges);
```

Add `db_query_count` and `db_table_count` to `EmitOutcome` for summary printing.

---

## Stable IDs

| Kind | Format | Example |
|---|---|---|
| DbQuery (constant) | `DbQuery:<owner_fqcn>#<const_name>` | `DbQuery:com.bank.OverdraftAdapterImpl#QUERY_GETCUSTOMOVERDRAFTTYPEBYCODE` |
| DbQuery (inline) | `DbQuery:<file>:<line>:<col>` | `DbQuery:src/.../Adapter.java:42:8` |
| DbTable | `DbTable:<UPPERCASE_TABLE>` | `DbTable:CUSTOM_OVERDRAFT_TYPE` |

---

## Test plan

### `crates/cih-core/src/lib.rs`
- `db_node_kind_labels_round_trip` — add DbQuery, DbTable to existing round-trip test
- `db_id_helpers_use_locked_scheme` — verify format strings for db_query_const_id, db_table_id

### `crates/cih-parse/src/sql.rs` (inline tests)
See "SQL scanner tests" above — ~10 inline unit tests.

### `crates/cih-parse/src/lib.rs` (integration)
`parses_sql_constants_from_static_final_string_fields`:
- File with `private static final String QUERY_FOO = "SELECT * FROM CUSTOM_OVERDRAFT WHERE id = ?";`
- Assert `sql_constants` has one entry: `const_name="QUERY_FOO"`, `sql_text` contains `SELECT`, `dynamic=false`

`parses_sql_constants_folds_string_concatenation`:
- `"SELECT * FROM " + "CUSTOM_OVERDRAFT " + "WHERE id = ?"`
- Assert `sql_text = "SELECT * FROM CUSTOM_OVERDRAFT WHERE id = ?"`, `dynamic=false`

`parses_sql_constants_marks_dynamic_on_non_literal_concat`:
- `"SELECT * FROM " + tableName`
- Assert `dynamic=true`, `sql_text` contains partial text

`parses_sql_execution_sites_dbutil_pattern`:
- Method with `DBUtil.executeQuery(conn, QUERY_FOO, params)` body
- Assert `sql_execution_sites` has one entry: `api_name="executeQuery"`, `const_ref=Some("QUERY_FOO")`

`parses_sql_execution_sites_jdbctemplate_pattern`:
- Method with `jdbcTemplate.query(QUERY_FOO, mapper)` where `jdbcTemplate: JdbcTemplate`
- Assert entry with `api_name="query"`, `const_ref=Some("QUERY_FOO")`

### `crates/cih-resolve/src/db_access.rs` (unit)
`emit_db_access_emits_query_table_nodes_and_edges`:
- Fixture `ParsedFile` with one `SqlConstant` (`SELECT * FROM TABLE_A WHERE x = ?`) and one `SqlExecutionSite` referencing it
- Assert: `DbQuery` node present; `DbTable:TABLE_A` node present; `EXECUTES_QUERY` edge; `READS_TABLE` edge

`emit_db_access_writes_table_uses_writes_table_edge`:
- `INSERT INTO TABLE_B (col) VALUES (?)` → `WRITES_TABLE` edge to `DbTable:TABLE_B`

`emit_db_access_deduplicates_db_table_nodes`:
- Two queries both reading `TABLE_A` → only one `DbTable:TABLE_A` node

`emit_db_access_skips_site_with_unknown_const_ref`:
- Execution site references `QUERY_FROM_OTHER_CLASS` not present in same file's constants
- Assert no DbQuery or DbTable nodes emitted for that site

`emit_db_access_marks_dynamic_in_props`:
- SqlConstant with `dynamic=true` → DbQuery node props has `"dynamic": true`

### `crates/cih-engine/src/tests.rs` (integration)
`analyze_emit_writes_db_query_and_table_artifacts`:
- Fixture file: `OverdraftAdapterImpl.java` with a static SQL constant + a DBUtil execution site
- Assert `nodes.jsonl` contains `DbQuery:...` and `DbTable:CUSTOM_OVERDRAFT_TYPE` entries
- Assert `edges.jsonl` contains `EXECUTES_QUERY` and `READS_TABLE` entries

### Run
```bash
~/.cargo/bin/cargo test -p cih-core
~/.cargo/bin/cargo test -p cih-parse
~/.cargo/bin/cargo test -p cih-resolve
~/.cargo/bin/cargo test -p cih-engine
~/.cargo/bin/cargo test --workspace
```

---

## Scope constraints (v1)

- **Same-class constant lookup only**: if `const_ref` points to a constant from another class,
  the site is emitted with `dynamic=true` and best-effort empty tables (no cross-file constant
  resolution in v1). Mark with `reason: "cross-class-const-unresolved"` in node props.
- **Column-level lineage**: out of scope
- **Full SQL parser**: not needed — conservative token scanner is sufficient for Oracle banking SQL

---

## Implementation order

1. `cih-core`: `NodeKind::DbQuery|DbTable`, `EdgeKind::ExecutesQuery|ReadsTable|WritesTable`, id helpers, `SqlConstant`/`SqlExecutionSite` IR structs, `ParsedFile` fields
2. `cih-parse/src/sql.rs`: SQL table scanner + inline unit tests
3. `cih-parse/src/java.rs`: `collect_sql_constants` + `collect_sql_execution_sites` + parser integration tests
4. `cih-resolve/src/db_access.rs`: `emit_db_access` + unit tests
5. `cih-engine/src/analyze.rs`: wire `emit_db_access` into the merge point + engine integration test
6. `cargo test --workspace` green
