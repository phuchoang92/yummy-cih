# Plan: Semantic Enrichment for Graph-Only `discover`

## Goal

Make `cih-engine discover` emit enough semantic metadata on `Community` and `Process` nodes
that the wiki is useful without LLM. After this plan, a no-LLM wiki should show:
- meaningful community names derived from routes, controllers, DB tables, or packages
- business processes that map to real HTTP flows or event flows
- PO/BA pages that show only actual business workflows, not test or internal utility traces

No new commands. No LLM dependency. No Leiden repartitioning.

---

## Architectural decision: push naming into the artifact, not the render

`cih-wiki/src/features.rs::infer_community_feature` currently re-derives feature names at
render time from routes, tables, and topics. This plan moves that derivation one step earlier:
`cih-community` writes `feature` and `display_name` into `Community.props` during
`detect_communities`. The wiki then **prefers the prop** and falls back to the existing
`infer_community_feature` for artifacts produced before this change.

**`infer_community_feature` is NOT removed** — it remains the fallback for backward compatibility.
Once all live artifacts are re-generated, it can be removed in a follow-up cleanup.

---

## `entrypoint_kind` values (canonical enum)

Every `Process` node will carry `entrypoint_kind` with one of these string values:

| Value | Meaning |
|---|---|
| `"http_route"` | Entry method has at least one outgoing `HandlesRoute` edge |
| `"event_listener"` | Entry method has at least one outgoing `ListensTo` edge |
| `"scheduled"` | Entry name matches scheduled-task patterns (`run`, `execute`, `schedule`, `batch`) with the same word-boundary rule as `starts_entry` (rest is empty, `_`, or uppercase) |
| `"main"` | Entry method is literally named `main` |
| `"fanout"` | Fallback: selected by high callees/callers ratio with no semantic signal |

`business_flow` is `true` iff `entrypoint_kind` is `"http_route"` or `"event_listener"`.
The whole trace inherits this flag from the entry — no per-step classification needed.

Add a second prop, `business_surface`, so scheduled and internal flows are still visible to Dev
without being mixed into PO/BA online workflows:

| `entrypoint_kind` | `business_surface` | `business_flow` |
|---|---|---|
| `"http_route"` | `"http"` | `true` |
| `"event_listener"` | `"event"` | `true` |
| `"scheduled"` | `"scheduled"` | `false` for now |
| `"main"` | `"main"` | `false` |
| `"fanout"` | `"internal"` | `false` |

Scheduled jobs can be real banking business flows, but this phase detects them only by weak name
patterns because CIH does not yet emit `@Scheduled` edges/props. Keep them out of PO/BA workflow
sections until a later parser phase can make scheduled detection annotation-backed.

---

## Changes

### 1. `cih-community/src/entry_points.rs` — edge-based entrypoint detection

**Current problem**: `score_entry_points` uses `name_multiplier(name)` — string matching only.
A route handler named `list` or `search` gets no boost. A utility named `handleException` gets
an undeserved one.

**Fix**: pass `edges` into `score_entry_points`. Classify each candidate by checking whether
it has an outgoing `HandlesRoute` or `ListensTo` edge. Use name patterns only as a secondary
signal when no edges are present.

```rust
pub fn score_entry_points(
    nodes: &[Node],
    edges: &[Edge],           // NEW parameter
    digraph: &DiGraph<NodeId, f32>,
    node_index: &HashMap<NodeId, NodeIndex>,
) -> Vec<ScoredEntrypoint>
```

`ScoredEntrypoint` keeps semantic metadata out of the BFS queue while preserving the current BFS
signature:

```rust
pub(crate) struct ScoredEntrypoint {
    pub id: NodeId,
    pub score: f64,
    pub kind: EntrypointKind,
    pub route_method: Option<String>,
    pub route_path: Option<String>,
    pub event_topics: Vec<String>,
}
```

Use an owned route metadata helper instead of borrowing route props out of the node map:

```rust
struct RouteInfo {
    method: String,
    path: String,
}
```

Build `HashMap<NodeId, RouteInfo>` keyed by handler method ID.

For event listeners, build `HashMap<NodeId, Vec<String>>` keyed by listener method ID from
outgoing `ListensTo` edges. Values are topic names/IDs from the destination `KafkaTopic` node,
sorted and deduplicated for deterministic output.

`EntrypointKind` (crate-private enum, serialized to the string values above):
```rust
pub(crate) enum EntrypointKind { HttpRoute, EventListener, Scheduled, Main, Fanout }
```

Scoring multipliers:
- `HttpRoute` or `EventListener` → `3.0`
- `Scheduled` or `Main` → `2.0`
- name-based entry heuristic (existing `is_entry_name`) → `1.5`, but the serialized kind
  remains `"fanout"` unless it specifically matched `Scheduled` or `Main`
- `Fanout` → `1.0`
- utility names → `0.3` (unchanged)

Semantic entrypoints with no resolved callees are still useful in banking repos with unresolved
dynamic/factory calls. Do not apply the current `callees == 0.0` skip to `HttpRoute` or
`EventListener` candidates — keep them in the scored list so `trace_processes` can handle them.
Keep the skip for `Scheduled`, `Main`, and `Fanout` candidates so generic leaf methods do not
flood process output. The actual shallow one-step process emission for zero-callee semantic
entries happens in `trace_processes` (section 2 step 3), not here.

Test methods are excluded entirely — score zero, never emitted as process entrypoints. Detection
must inspect the method and its enclosing class:

- file path contains `/test/` or `/src/test/`;
- method node prop `isTest: true`;
- method name ends with `Test`, `Tests`, `IT`, or `Spec`;
- enclosing class parsed from `Method:pkg.Class#method/arity` or
  `Constructor:pkg.Class#<init>/arity` ends with `Test`, `Tests`, `IT`, or `Spec`;
- enclosing class node exists and has `props.stereotype == "test"`.

### 2. `cih-community/src/lib.rs::trace_processes` — emit `entrypoint_kind` and `business_flow`

`trace_processes` already receives `edges: &[Edge]`; keep its public signature unchanged.
After this change it calls `score_entry_points(nodes, edges, ...)`, then:

1. builds a lookup map keyed by entry `NodeId` for `ScoredEntrypoint`;
2. projects candidates into `Vec<(NodeId, f64)>` before calling
   `bfs::trace_process_paths(...)`, preserving the current BFS API;
3. if a semantic `HttpRoute` or `EventListener` entry has no accepted BFS trace, emits a shallow
   one-step process for that entrypoint so the wiki can still show the route/event as a business
   workflow with incomplete internals. This is the only exception to `ProcessConfig.min_steps`;
   generic fanout/internal processes still obey `min_steps`;
4. writes semantic entrypoint props onto each `Process` node.

```rust
props: Some(serde_json::json!({
    "label":            label,
    "process_type":     if cross_community { "cross_community" } else { "intra_community" },
    "step_count":       trace_ids.len(),
    "communities":      communities,
    "entry_point_id":   entry_id.as_str(),
    "terminal_id":      terminal_id.as_str(),
    // NEW:
    "entrypoint_kind":  entrypoint_kind_str,   // one of the five values above
    "business_flow":    business_flow,          // true iff http_route or event_listener
    "business_surface": business_surface,       // http | event | scheduled | main | internal
    "route_path":       route_path,             // Option<String> / JSON null if none
    "route_method":     route_method,           // Option<String> / JSON null if none
    "event_topics":     event_topics,           // Vec<String>; non-empty for event_listener
}))
```

`route_path` and `route_method` come from the `ScoredEntrypoint` metadata populated from
`HandlesRoute` edges where the entry node is the edge source. Because `HandlesRoute` edges
live in the flat `edges: &[Edge]` slice (not in `digraph`, which is calls-only),
`score_entry_points` pre-indexes them at the top of the function:
`let route_edges: HashMap<NodeId, RouteInfo>` keyed by handler node ID, storing owned
`method` and `path` strings extracted from the destination `Route` node's props.

`event_topics` comes from the `ScoredEntrypoint` metadata populated from `ListensTo` edges where
the entry node is the edge source. Use the destination `KafkaTopic` node name when available,
otherwise strip the `KafkaTopic:` prefix from the destination ID.

### 3. `cih-community/src/lib.rs::detect_communities` — semantic community facts pass

After Leiden clustering emits `CommunityOutput`, add a deterministic enrichment pass that
sets additional props on each `Community` node. This pass has read access to `nodes` and
`edges` (already in scope).

New props written (all additive — existing `label`, `heuristic_label`, `cohesion`,
`symbol_count`, `color` are preserved):

```rust
{
    "display_name":       String,          // best human name (see naming precedence below)
    "feature":            String,          // slug used for wiki feature grouping
    "naming_reason":      String,          // e.g. "route_prefix", "controller", "db_table", ...
    "route_prefixes":     Vec<String>,     // distinct non-generic route prefix segments
    "controllers":        Vec<String>,     // simple controller class names
    "db_tables":          Vec<String>,     // table names accessed by any member
    "topics":             Vec<String>,     // Kafka topic IDs published or consumed
    "primary_stereotype": Option<String>,  // most common stereotype label, if any
}
```

**Naming precedence** (first that yields a non-empty, non-generic result wins):

1. **Route prefix** — gather all `Route` nodes reachable via `HandlesRoute` from member
   methods; extract the first non-generic, non-version, non-param path segment
   (same logic as `features.rs::route_feature`). `naming_reason = "route_prefix"`.
2. **Controller class** — if any member's `id` parses to a class ending in `Controller` or
   `Resource`, use the part before that suffix. `naming_reason = "controller"`.
3. **DB table prefix** — find tables accessed by members via `ExecutesQuery → ReadsTable /
   WritesTable`; extract the first non-generic token from the most-common table name.
   `naming_reason = "db_table"`.
4. **Topic prefix** — topic IDs from `PublishesEvent` / `ListensTo` edges; extract first
   non-generic token. `naming_reason = "topic"`.
5. **Package/folder** — existing `label::heuristic_label` logic.
   `naming_reason = "folder"`.
6. **Fallback** — `Cluster_N`. `naming_reason = "fallback"`.

All fact aggregation must be deterministic:

- collect unique values in `BTreeSet`;
- count candidate names in `BTreeMap<String, usize>`;
- choose winners by count descending, then lexical ascending;
- sort prop arrays (`route_prefixes`, `controllers`, `db_tables`, `topics`) lexically before
  writing JSON.

`feature` is the slugified form of `display_name` (lowercase, hyphens), **except** when
`naming_reason = "fallback"` — in that case store `feature = ""` so the wiki fast-path skips
it and falls back to existing `infer_community_feature` inference rather than creating
`cluster-5/po/...` pages.
Set `Community.name` and `qualified_name` to `display_name` so old consumers also get the better
label, while preserving the original folder/path label in both `label` and `heuristic_label` props.

`detect_communities` signature is unchanged — `nodes` and `edges` are already parameters
via the existing call chain; the enrichment pass reads them in-place before returning.

### 4. `cih-wiki/src/features.rs` — prefer enriched props, keep fallback

`infer_community_feature` gains a fast-path:

```rust
pub fn infer_community_feature(community_id: &str, graph: &WikiGraph) -> String {
    // Fast-path: use prop written by cih-community if present
    if let Some(feature) = graph.nodes_by_id.get(community_id)
        .and_then(|n| n.props.as_ref())
        .and_then(|p| p.get("feature"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "shared")
    {
        return feature.to_string();
    }
    // Existing inference logic unchanged below...
}
```

`WikiGraph` gains a `community_display_name` helper:

```rust
pub fn community_display_name<'a>(&'a self, community_id: &'a str) -> &'a str {
    self.nodes_by_id.get(community_id)
        .and_then(|n| n.props.as_ref())
        .and_then(|p| p.get("display_name"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| self.community_name(community_id))
}
```

Wiki page renderers call `community_display_name` for headings and table cells, keeping
`community_name` for IDs/anchors and backward compatibility. For artifacts produced before
this change, `Node.name` will still be the heuristic folder label; the helper reads
`props.display_name` first to handle both old and new artifacts transparently.

### 5. `cih-wiki` PO/BA feature pages and diagrams — filter on `business_flow`

The current primary wiki flow renders feature-level pages, so filtering must happen in:

- `cih-wiki/src/pages/feature_po.rs`
- `cih-wiki/src/pages/feature_ba.rs`
- `cih-wiki/src/mermaid.rs`

Add a graph helper:

```rust
impl WikiGraph {
    pub fn is_business_process(&self, process_id: &str) -> bool {
        self.nodes_by_id
            .get(process_id)
            .and_then(|n| n.props.as_ref())
            .and_then(|p| p.get("business_flow"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }
}
```

Migration behavior: for old community artifacts whose `Process` nodes do not have a
`business_flow` prop, `is_business_process` returns `false`. This intentionally prevents old
fanout/test traces from appearing in PO/BA pages. Users should rerun `cih-engine discover <repo>`
after this change to regenerate enriched process metadata. Dev pages keep showing all existing
processes, so old artifacts remain inspectable.

Add a shared helper on `WikiGraph` so both feature page renderers use the same logic:

```rust
impl WikiGraph {
    pub fn processes_for_community(&self, community_id: &str, business_only: bool) -> Vec<String>
}
```

When `business_only` is true, only include `Process` nodes whose prop `business_flow == true`.
`feature_po.rs` calls this for process counts. `feature_ba.rs` calls this for workflow sections.
The existing private `processes_for_community` in `po.rs` is kept unchanged for the older role
pages — do not modify it. `mermaid::process_flow_diagram` also accepts a `business_only` flag so
BA diagrams do not include internal/test/fanout traces. Dev pages keep existing behavior and may
show all processes, including `business_surface = "scheduled"` and `"internal"`.

---

## What stays the same

| Component | Reason unchanged |
|---|---|
| Leiden clustering | No repartitioning; enrichment is metadata only |
| `cih-parse`, `cih-resolve`, `cih-core` | No IR changes |
| `cih-engine` commands | Same `discover` and `wiki` invocations |
| `infer_community_feature` body | Kept as fallback; only a fast-path prepended |
| `cih-wiki` feature grouping logic | Unchanged; just preferring a prop if set |
| `bfs::trace_process_paths` signature | Unchanged; `trace_processes` projects semantic candidates to `(NodeId, f64)` |
| Artifact version scheme | Props are additive; old readers ignore unknown keys |

---

## Files changed

| File | Change |
|---|---|
| `crates/cih-community/src/entry_points.rs` | Add `edges` param; edge-based classification; `ScoredEntrypoint`; `EntrypointKind`; exclude test methods |
| `crates/cih-community/src/lib.rs` | Project semantic candidates into BFS input; emit new process props; `detect_communities` gains enrichment pass + new community props |
| `crates/cih-wiki/src/features.rs` | Fast-path in `infer_community_feature` |
| `crates/cih-wiki/src/graph.rs` | Add `community_display_name` and `is_business_process` helpers |
| `crates/cih-wiki/src/pages/feature_po.rs` | Use business-only process counts; use `community_display_name` where community names are shown |
| `crates/cih-wiki/src/pages/feature_ba.rs` | Business-only workflows; use `community_display_name` |
| `crates/cih-wiki/src/mermaid.rs` | Add business-only process filtering for BA diagrams |
| `crates/cih-engine/src/discover.rs` | No signature change expected; keep passing `edges` to existing `trace_processes` |

---

## Test plan

### `cih-community` tests

| Test | Assertion |
|---|---|
| Route handler has `HandlesRoute` edge → `entrypoint_kind = "http_route"`, `business_flow = true` | score > fanout fallback |
| Event listener has `ListensTo` edge → `entrypoint_kind = "event_listener"`, `business_flow = true`, `event_topics` populated | |
| Method named `processOrder` with no route/event edge → `entrypoint_kind = "fanout"` or secondary entry, `business_flow = false` | name alone is not enough for PO/BA |
| Scheduled-looking method without annotation-backed evidence → `business_surface = "scheduled"`, `business_flow = false` | kept visible to Dev, hidden from PO/BA |
| Method with `isTest: true` prop → excluded from entrypoints entirely | |
| Method in `/test/` file path → excluded from entrypoints entirely | |
| Route `/api/v1/orders/{id}` → community `display_name = "Orders"`, `feature = "orders"`, `naming_reason = "route_prefix"` | |
| Controller class `CartController`, no routes → `display_name = "Cart"`, `naming_reason = "controller"` | |
| DB table `CUSTOM_OVERDRAFT`, no routes/controllers → `display_name = "Custom"` or `"Overdraft"`, `naming_reason = "db_table"` | |
| No routes, tables, topics, meaningful folder → `display_name = "Cluster_N"`, `naming_reason = "fallback"` | full waterfall |
| Process node has `business_surface`, `route_path`, and `route_method` props when entry handles a route | |
| Process node has sorted `event_topics` props when entry listens to topics | |
| Community fact tie-breaks are deterministic: count descending, lexical ascending | |
| `trace_process_paths` API remains unchanged and receives projected `(NodeId, f64)` candidates | |

### `cih-wiki` tests

| Test | Assertion |
|---|---|
| Community with `props.feature = "payment"` → `infer_community_feature` returns `"payment"` without checking routes | fast-path |
| Community with no `props.feature` → existing inference runs unchanged | backward compat |
| `community_display_name` prefers `props.display_name` over `node.name` | |
| Feature PO page with `business_flow = true` processes → process count includes them | |
| Feature PO page with only `business_flow = false` processes → process count is zero | |
| Feature BA page hides test/internal/fanout processes; shows only `business_flow = true` | |
| BA Mermaid process diagram hides non-business processes when `business_only = true` | |

### Integration

```bash
cargo test -p cih-community
cargo test -p cih-wiki
cargo test -p cih-engine
cargo test --workspace
```

Run `discover` on a real repo and verify:
- community names are route/controller-derived, not `Cluster_N`
- PO pages show HTTP-route-backed processes only
- BA process diagrams show HTTP/event-backed flows only
- Dev pages still show all processes, including scheduled/internal traces

---

## Out of scope

- Merging or splitting Leiden communities
- LLM enrichment
- Python/Go language support
- Scheduled-task trace inference beyond name patterns
