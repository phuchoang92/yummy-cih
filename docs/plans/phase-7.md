# Phase 7 — Spring: @Bean + JPA Repository + route_map (detailed plan)

Phase 3 delivered stereotypes, Route nodes, and HANDLES_ROUTE edges.
Phase 7 closes the three remaining Spring items from the ROADMAP — all additive, no schema changes.

---

## Goals

1. **`@Bean` producer detection** — tag `@Configuration` methods with `@Bean` so the bean wiring
   graph is queryable before Phase 13's full DI-aware resolution.
2. **JPA repository-interface tagging** — classes that `implements JpaRepository<T,ID>` (or any
   Spring Data base interface) are repositories even without `@Repository`; extract the entity type
   parameter for free.
3. **`route_map` MCP tool** — aggregated API surface view: `path + HTTP method → handler method`,
   with an optional path-prefix filter. Ports the concept from GitNexus's `route_map` tool
   (`gitnexus/src/mcp/local/local-backend.ts:4961–4993`).

---

## What Already Exists (do not re-implement)

| Item | Location | Status |
|------|----------|--------|
| `NodeKind::Route` | `cih-core/src/lib.rs:50` | ✅ |
| `EdgeKind::HandlesRoute` | `cih-core/src/lib.rs:154` | ✅ |
| Stereotype prop (controller/service/repository/…) | `cih-parse/src/java.rs:1074` | ✅ |
| Route node + HANDLES_ROUTE edge creation | `cih-parse/src/java.rs:619–670` | ✅ |
| `spring_method_routes()` annotation reader | `cih-parse/src/java.rs:679` | ✅ |
| `class_stereotype()` annotation reader | `cih-parse/src/java.rs:1074` | ✅ |
| `GraphStore::communities()` pattern | `cih-graph-store/src/lib.rs` | ✅ reuse for route_map |
| `FalkorStore::communities()` Cypher pattern | `cih-falkor/src/lib.rs` | ✅ reuse for route_map |
| MCP tool pattern (`#[tool]` + `Parameters`) | `cih-server/src/main.rs` | ✅ reuse |

No new crates. No new `NodeKind` or `EdgeKind` variants.

---

## Feature 1 — `@Bean` Producer Detection

### Where: `crates/cih-parse/src/java.rs`

Add a function that reads a method's annotation list (same pattern as `spring_method_routes()`
at line 679) and returns `true` if any annotation name is `"Bean"`:

```rust
fn is_bean_method(method_node: tree_sitter::Node<'_>, src: &str) -> bool
```

Walk the method's `modifiers` node, find `annotation` children, check
`annotation.child_by_field_name("name")` text == `"Bean"`.

In `collect_method()`, call `is_bean_method(...)` and, if true:
```rust
props["isBean"] = serde_json::Value::Bool(true)
```

`props` is already a `serde_json::Map<String, Value>` on `SymbolDef`. The method node carries
this map to FalkorDB via `bulk_load`. **No new NodeKind, EdgeKind, or schema change.**

---

## Feature 2 — JPA Repository-Interface Tagging

### Where: `crates/cih-parse/src/java.rs`

JPA base interfaces to detect (short name, no import resolution needed):

```rust
const JPA_INTERFACES: &[&str] = &[
    "JpaRepository",
    "CrudRepository",
    "PagingAndSortingRepository",
    "ListCrudRepository",
    "ListPagingAndSortingRepository",
    "MongoRepository",
    "ReactiveCrudRepository",
    "ReactiveMongoRepository",
    "R2dbcRepository",
    "JpaSpecificationExecutor",
];
```

### Tree-sitter traversal

Add a separate function (keep `class_stereotype()` unchanged):

```rust
/// Returns (is_jpa_repo, entity_type_short_name).
/// entity_type is the first generic type parameter, e.g. "User" from JpaRepository<User, Long>.
fn jpa_repository_props(class_node: tree_sitter::Node<'_>, src: &str)
    -> Option<(bool, Option<String>)>
```

Tree-sitter Java grammar shape for `implements JpaRepository<User, Long>`:
```
class_declaration
  → interfaces: super_interfaces
    → type_list
        → generic_type
             name: type_identifier        // "JpaRepository"
             type_arguments: type_arguments
               → type_identifier          // "User"  (first arg = entity type)
               → type_identifier          // "Long"  (second arg = ID type, ignored)
```

**Algorithm:**
1. `class_node.child_by_field_name("interfaces")` — skip if absent.
2. Walk `type_list` children:
   - `type_identifier` → short name = `src[child.byte_range()]`
   - `generic_type` → `name` child gives short name; first `type_argument` gives entity type
3. If any short name is in `JPA_INTERFACES` → return `Some((true, entity_type_opt))`.

**Integration in `collect_class()`:**
```
let stereotype = class_stereotype(&class_node, src);
let jpa = jpa_repository_props(&class_node, src);

if let Some((true, entity_opt)) = jpa {
    if stereotype.is_none() {
        // Force repository stereotype when only the interface signals it
        props.insert("stereotype", json!("repository"));
    }
    if let Some(entity) = entity_opt {
        props.insert("entityType", json!(entity));
    }
}
```

---

## Feature 3 — `route_map` MCP Tool

### 3a. `RouteInfo` + trait method — `cih-graph-store/src/lib.rs`

```rust
pub struct RouteInfo {
    pub path:              String,
    pub http_method:       String,
    pub decorator:         String,
    pub handler_id:        NodeId,
    pub handler_name:      String,
    pub handler_qualified: String,
}
```

Add to the `GraphStore` trait (after `communities`):
```rust
async fn route_map(
    &self,
    prefix: Option<&str>,
    limit: usize,
) -> Result<Vec<RouteInfo>>;
```

### 3b. FalkorDB implementation — `cih-falkor/src/lib.rs`

Cypher query (note: FalkorDB uses inline `CYPHER` parameter syntax as established in Phase 2):
```cypher
CYPHER prefix=$prefix limit=$limit
MATCH (m:Symbol)-[:HANDLES_ROUTE]->(r:Symbol)
WHERE r.kind = 'Route'
  AND ($prefix = '' OR r.path STARTS WITH $prefix)
RETURN r.path, r.httpMethod, r.decorator, r.handler, m.id, m.name, m.qualifiedName
ORDER BY r.path, r.httpMethod
LIMIT $limit
```

Map each row to `RouteInfo` using the existing `cell_to_string()` helper. Same pattern as
`communities()` at `cih-falkor/src/lib.rs` (query → rows → map to struct → return Vec).

### 3c. MCP tool — `cih-server/src/main.rs`

```rust
#[derive(Debug, Deserialize, JsonSchema)]
struct RouteMapArgs {
    /// Path prefix filter (e.g. "/api/owners"). Omit for all routes.
    #[serde(default)]
    prefix: String,
    /// Max routes to return (default 200).
    #[serde(default)]
    limit: Option<usize>,
}

#[tool(description = "List Spring REST endpoints: HTTP method + path + handler method. \
    Use prefix to filter by path (e.g. prefix=\"/api/users\").")]
async fn route_map(
    &self,
    Parameters(args): Parameters<RouteMapArgs>,
) -> Result<CallToolResult, McpError>
```

Implementation: `self.store.route_map(prefix_opt, limit).await?` → serialize to JSON.
Same pattern as the `communities` tool.

---

## Tests (8 required)

### `cih-parse` unit tests (5) — `crates/cih-parse/src/lib.rs`

1. **`bean_method_tagged_when_annotated`** — `@Configuration` class with one `@Bean` method and
   one plain method; verify `@Bean` method has `props.isBean == true`; plain method has no `isBean`.

2. **`bean_method_not_tagged_without_annotation`** — plain class with `produce()` method; no `isBean`.

3. **`jpa_repository_tagged_as_repository`** — `class UserRepo implements JpaRepository<User, Long>`;
   verify stereotype == `"repository"` and `entityType` prop == `"User"`.

4. **`jpa_crud_repository_also_tagged`** — `class ItemRepo implements CrudRepository<Item, UUID>`;
   verify stereotype == `"repository"`.

5. **`jpa_annotation_idempotent_with_interface`** — class has both `@Repository` annotation AND
   `implements JpaRepository<…>`; verify stereotype is `"repository"` (not double-set).

### `cih-falkor` unit tests (2) — `crates/cih-falkor/src/lib.rs`

6. **`route_map_row_parses_correctly`** — construct a mock FalkorValue row; call the row→RouteInfo
   helper; verify all fields populated correctly.

7. **`route_map_empty_result_returns_empty_vec`** — empty row set → empty `Vec<RouteInfo>`, no panic.

### `cih-server` unit test (1) — `crates/cih-server/src/main.rs`

8. **`route_map_args_default_limit_is_none`** — deserialize `RouteMapArgs` from `{}`; verify
   `prefix.is_empty()` and `limit == None`.

---

## Sequencing

1. **cih-parse** — `is_bean_method()` + `jpa_repository_props()`, integrate into
   `collect_method` / `collect_class`. Add 5 tests. `cargo test -p cih-parse` green.

2. **cih-graph-store** — add `RouteInfo` + `route_map()` to GraphStore trait.

3. **cih-falkor** — implement `FalkorStore::route_map()`. Add 2 tests.
   `cargo test -p cih-falkor` green.

4. **cih-server** — add `route_map` MCP tool. Add 1 test.
   `cargo test -p cih-server` green.

5. **ROADMAP.md** — mark Phase 7 ✅ with final test count.

---

## Verification (end-to-end)

```bash
# 1. Re-analyze a Spring Boot repo (picks up new @Bean + JPA props)
cargo run -p cih-engine -- analyze <spring-repo> --all

# 2. Start MCP server
FALKOR_URL=redis://127.0.0.1:6380 cargo run -p cih-server

# 3. MCP Inspector checks
# → route_map({})                  → all routes returned, ordered by path
# → route_map({prefix: "/owners"}) → only /owners/* routes
# → context("Method:…UserRepo#findById/1")
#      → node.stereotype == "repository", node.entityType == "User"
# → context("Method:…AppConfig#dataSource/0")
#      → node.isBean == true

# 4. All tests green
cargo test --workspace
cargo clippy --workspace
```

---

## Critical Files Summary

| Action | File | Change |
|--------|------|--------|
| **Edit** | `crates/cih-parse/src/java.rs` | Add `is_bean_method()`, `jpa_repository_props()`, integrate |
| **Edit** | `crates/cih-parse/src/lib.rs` | Add 5 unit tests |
| **Edit** | `crates/cih-graph-store/src/lib.rs` | Add `RouteInfo` + `route_map()` to trait |
| **Edit** | `crates/cih-falkor/src/lib.rs` | Implement `route_map()`; add 2 unit tests |
| **Edit** | `crates/cih-server/src/main.rs` | Add `route_map` MCP tool + 1 unit test |
| **Edit** | `ROADMAP.md` | Mark Phase 7 ✅ |

---

## Risks / Decisions

- **`@Bean` scope/qualifiers ignored:** `@Scope("prototype")` and `@Qualifier` are Phase 13 concerns.
- **JPA short-name detection:** Checking raw short names (e.g., `JpaRepository`) without import
  resolution is safe in practice — Spring Data interface names are globally unique by convention.
- **`GraphStore::route_map()` on non-Falkor adapters:** Return `Err(GraphStoreError::Unimplemented)`
  until Phase 11 adds Postgres/Neptune adapters (same pattern used by `communities()` pre-Phase 5).
- **`route_map` is an index, not a traversal:** The handler → service call chain is left to the
  `context` tool. Phase 7's tool is purely an API surface view.
