# Phase 17 — Visualization Output for yummy Frontend

## Goal

Add a `format` parameter to four existing MCP tools so the yummy frontend can render
architecture diagrams without a separate rendering backend. No new graph data is needed —
this is purely output-format additions.

| Tool | New format value | Output |
|------|-----------------|--------|
| `trace_flow` | `"mermaid"` | Mermaid `flowchart TD` of the execution chain |
| `impact` | `"diagram"` | D3-JSON force-directed blast-radius graph |
| `communities` | `"diagram"` | D3-JSON service map with inter-community edge weights |
| `route_map` | `"openapi"` | OpenAPI 3.0 JSON of the indexed route surface |

---

## Data gaps — what needs to change before rendering

The Mermaid and D3 renderers need edge (parent→child) relationships, not just node lists.
The current Falkor queries for `flow_downstream` and `impact` return a flat list of nodes with
depth but no parent tracking. Two struct changes fix this (additive, serde-safe):

### `cih-graph-store/src/lib.rs`

**`FlowNode`** — add one optional field:
```rust
pub parent_id: Option<NodeId>,   // id of the predecessor node in the shortest path
```

**`ImpactNode`** — add three fields (enriches the existing JSON output for all callers):
```rust
pub name: String,                // short name of the affected node (was always "CALLS" via)
pub kind: String,                // node kind label (e.g. "Method", "Class")
pub parent_id: Option<NodeId>,   // predecessor in the shortest upstream/downstream path
```

**New struct `CommunityEdge`** — for the community service-map diagram:
```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommunityEdge {
    pub src: String,    // Community node id
    pub dst: String,    // Community node id
    pub weight: u64,    // number of inter-community CALLS edges between their members
}
```

**New `GraphStore` trait method:**
```rust
/// Return all inter-community call edges: (src_community_id, dst_community_id, call_count).
/// A CALLS edge from a member of community A to a member of community B = one unit of weight.
async fn community_graph(&self) -> Result<Vec<CommunityEdge>>;
```

---

## Falkor Cypher changes (`cih-falkor/src/lib.rs`)

### `flow_downstream()` — add parent tracking

Replace the current query with a two-step aggregation that picks the parent from the
shortest path:

```cypher
CYPHER id='...'
MATCH p=(start:Symbol {id:$id})
      -[:CALLS|HANDLES_ROUTE|EXTERNAL_CALL|PUBLISHES_EVENT|LISTENS_TO*1..{d}]->(m:Symbol)
WITH m, length(p) AS len, nodes(p)[length(p)-1] AS pnode
ORDER BY m.id, len
WITH m, collect(pnode)[0] AS parent, min(len) AS depth
RETURN m.id, m.kind, m.name, m.qualifiedName, m.file, depth, parent.id
ORDER BY depth, m.name LIMIT 100
```

Map the 7th column (`parent.id`) into `FlowNode.parent_id`.

### `impact()` — add name, kind, parent tracking

Replace the current query (direction-dependent arrow is unchanged):

```cypher
CYPHER id='...'
MATCH p=(n:Symbol {id:$id}){arrow}(m:Symbol)
WITH m, length(p) AS len, nodes(p)[length(p)-1] AS pnode
ORDER BY m.id, len
WITH m, collect(pnode)[0] AS parent, min(len) AS depth
RETURN m.id, depth, parent.id, m.name, labels(m)[0]
LIMIT 200
```

Map into `ImpactNode { id, depth, via: "CALLS", name, kind, parent_id }`.

### `community_graph()` — new method

```cypher
MATCH (a:Symbol)-[:MEMBER_OF]->(ca:Symbol),
      (b:Symbol)-[:MEMBER_OF]->(cb:Symbol)
WHERE ca.kind = 'Community' AND cb.kind = 'Community'
  AND (a)-[:CALLS]->(b) AND ca.id <> cb.id
RETURN ca.id, cb.id, count(*) AS weight
LIMIT 500
```

Map into `Vec<CommunityEdge>`.

---

## New file: `cih-server/src/viz.rs`

Pure rendering functions — no async, no store access. All take already-fetched data.

```rust
pub fn render_mermaid_flow(entry_id: &NodeId, steps: &[FlowNode]) -> String
```
- Outputs `flowchart TD\n` followed by node definitions and edges.
- Each `FlowNode` becomes `N{id_hash}["«Kind»\nname"]`.
- Entry point node is always included at depth 0 (not in `steps`).
- Edges are drawn from `parent_id` to each node. Nodes with `parent_id = None` connect to entry.
- Node IDs sanitized: replace non-alphanumeric with `_`, prefix with `n` to avoid leading digit.
- Truncate label at 40 chars with `…` to keep diagram readable.

```rust
pub fn render_d3_impact(impact: &Impact) -> serde_json::Value
```
- Returns `{ "format": "d3-force", "risk": "...", "nodes": [...], "links": [...] }`.
- Root node included in `nodes` with `"depth": 0`.
- Each `ImpactNode` → `{"id": "...", "label": name, "kind": kind, "depth": N}`.
- Each `ImpactNode` with `parent_id` → `{"source": parent_id, "target": id, "label": "CALLS"}`.
- Nodes with no `parent_id` → link from root to that node.

```rust
pub fn render_community_diagram(
    communities: &[CommunityInfo],
    edges: &[CommunityEdge],
) -> serde_json::Value
```
- Returns `{ "format": "d3-force", "nodes": [...], "links": [...] }`.
- Nodes: `{"id": community.id, "label": community.name, "symbol_count": N, "cohesion": F}`.
- Links: `{"source": src, "target": dst, "weight": N}`.
- Only include communities that appear in at least one edge or have `symbol_count > 0`.

```rust
pub fn render_openapi(routes: &[RouteInfo]) -> serde_json::Value
```
- Returns a valid OpenAPI 3.0.3 JSON object.
- Groups `RouteInfo` entries by `path` → method.
- Each method entry: `{ "operationId": <derived>, "summary": handler_name, "x-handler-id": handler_id, "x-handler-class": <derived from handler_qualified>, "responses": { "200": { "description": "OK" } } }`.
- `operationId` = snake_case(http_method) + `_` + path segments joined with `_`, truncated at 64 chars.
- Path variables stay as `{varName}` (OpenAPI compatible).
- No request body or response schema (data not available at this layer).

---

## `cih-server/src/main.rs` changes

### Arg struct additions

Add `format: Option<String>` to:
- `ImpactArgs`
- `TraceFlowArgs`
- `CommunitiesArgs`
- `RouteMapArgs`

```rust
/// Output format. Omit for default JSON. Pass "mermaid" (trace_flow),
/// "diagram" (impact, communities), or "openapi" (route_map).
#[serde(default)]
format: Option<String>,
```

### Dispatch in tool handlers

**`impact()`:**
```rust
if args.format.as_deref() == Some("diagram") {
    return text_result(serde_json::to_string_pretty(&render_d3_impact(&res))?);
}
json_result(&res)
```

**`trace_flow()`:**
```rust
if args.format.as_deref() == Some("mermaid") {
    return text_result(render_mermaid_flow(&id, &steps));
}
```

**`communities()`:**
```rust
if args.format.as_deref() == Some("diagram") {
    let edges = self.store.community_graph().await.map_err(to_mcp)?;
    return json_result(&render_community_diagram(&communities, &edges));
}
```

**`route_map()`:**
```rust
if args.format.as_deref() == Some("openapi") {
    return json_result(&render_openapi(&routes));
}
```

Add a `text_result()` helper (returns `CallToolResult` with a text content block instead of JSON):
```rust
fn text_result(s: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(s)]))
}
```

---

## Test plan

### Unit tests in `cih-server/src/viz.rs`

Four inline fixture tests, no async, no store:

1. **`render_mermaid_flow_empty`** — empty `steps` → Mermaid string contains entry node and
   no dangling edges.
2. **`render_mermaid_flow_two_hops`** — 2 FlowNodes with depth 1 and 2, parent_id set →
   Mermaid contains `-->` edges and both node labels.
3. **`render_d3_impact_produces_nodes_and_links`** — `Impact` with 2 `ImpactNode`s →
   `nodes` length is 3 (root + 2), `links` length is 2.
4. **`render_openapi_groups_by_path`** — two `RouteInfo`s sharing the same path with different
   HTTP methods → single path key with two method keys in output.
5. **`render_community_diagram_produces_nodes_and_links`** — 2 communities + 1 edge → correct
   node/link counts.

### Server arg tests in `cih-server/src/main.rs`

- `impact_args_accepts_format_diagram` — parse JSON with `"format": "diagram"` → `format` is `Some("diagram")`.
- `trace_flow_args_accepts_format_mermaid` — same pattern.

### Falkor unit tests in `cih-falkor/src/lib.rs`

- `community_graph_row_parses_correctly` — construct a 3-element row `["Community:A", "Community:B", "12"]` → `CommunityEdge { src: "Community:A", dst: "Community:B", weight: 12 }`. (Mirrors existing `route_map_row_parses_correctly` pattern.)

---

## Files to modify / create

| File | Change |
|---|---|
| `crates/cih-graph-store/src/lib.rs` | Add `parent_id` to `FlowNode`; add `name`, `kind`, `parent_id` to `ImpactNode`; add `CommunityEdge` struct; add `community_graph()` to `GraphStore` trait |
| `crates/cih-falkor/src/lib.rs` | Update `impact()` Cypher + row mapping; update `flow_downstream()` Cypher + row mapping; implement `community_graph()` |
| `crates/cih-server/src/viz.rs` *(new)* | 4 render functions + 5 unit tests |
| `crates/cih-server/src/main.rs` | `format` field on 4 arg structs; dispatch in 4 tool handlers; `text_result()` helper; `mod viz;` declaration; 2 new arg tests |

---

## Implementation order

1. `cih-graph-store`: struct additions + trait method
2. `cih-falkor`: Cypher updates + `community_graph()` impl + 1 unit test
3. `cih-server/src/viz.rs`: all 4 render functions + 5 unit tests
4. `cih-server/src/main.rs`: wire format dispatch + `text_result()` helper + 2 arg tests
5. `cargo test --workspace` — target ≥ 134 tests green
6. Update ROADMAP Phase 17 ✅
7. Commit

---

## Notes

- All format=omit paths are untouched — the `Option<String>` default is `None`, so all
  existing callers and tests continue to work without modification.
- The Mermaid output is returned as a text content block (not JSON) so the yummy frontend
  can pass it directly to a Mermaid renderer without parsing.
- The D3 and OpenAPI outputs are returned as JSON content blocks so they can be processed
  programmatically.
- `community_graph()` returns an empty vec when no `discover` run has been done (no
  Community nodes in the graph) — the diagram renderer shows a "no communities yet" empty
  state gracefully.
- OpenAPI does not include request/response schemas; those require Parse layer extensions
  (a future phase). The `x-handler-id` extension fields give yummy enough to deep-link into
  the code graph from an OpenAPI viewer.
