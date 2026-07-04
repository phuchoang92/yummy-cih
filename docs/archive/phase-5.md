# Phase 5 — Communities + Processes (detailed plan)

Goal: layer two higher-order structures on top of Phase 4's accurate call/heritage graph —
**Community** nodes (Leiden-style modularity clusters) and **Process** nodes (BFS execution-flow
traces) — loaded through the existing Phase-2 bulk-load path. Acceptance: `cih-engine discover
<repo>` populates FalkorDB with cluster and flow data; MCP `context` returns populated
`processes[]`; new MCP `communities` tool lists clusters.

Builds on Phase 4: the resolved `CALLS`/`EXTENDS`/`IMPLEMENTS` edges in `.cih/artifacts/<v>/` and
the `GraphArtifacts` JSONL I/O already established. Ports algorithm constants from GitNexus
(`src/core/ingestion/community-processor.ts` and `process-processor.ts`); all numeric parameters
below come from those files.

New crate: `cih-community`. Engine gains a `discover` subcommand. FalkorDB `context()` is updated to
return processes. MCP server gains a `communities` tool.

---

## What already exists (do not re-implement)

| Item | Location | Status |
|------|----------|--------|
| `NodeKind::Community` + `NodeKind::Process` | `cih-core/src/lib.rs` | ✅ defined |
| `EdgeKind::MemberOf` → `"MEMBER_OF"` | `cih-core/src/lib.rs` | ✅ defined |
| `EdgeKind::StepInProcess` → `"STEP_IN_PROCESS"` | `cih-core/src/lib.rs` | ✅ defined |
| `SymbolContext.processes: Vec<String>` | `cih-graph-store/src/lib.rs` | ✅ placeholder (`vec![]` + TODO) |
| `FalkorStore` MEMBER_OF / STEP_IN_PROCESS label parsing | `cih-falkor/src/lib.rs` | ✅ already wired |
| `GraphArtifacts::write/read_nodes/read_edges` | `cih-core` artifacts.rs | ✅ JSONL I/O |
| `GraphStore::bulk_load` | `cih-graph-store/src/lib.rs` | ✅ loads any nodes/edges |

**Need to add to `cih-core/src/lib.rs`:**
```rust
pub fn community_id(idx: usize) -> NodeId {
    NodeId(format!("Community:{idx}"))
}
pub fn process_id(entry_slug: &str, hash: &str) -> NodeId {
    NodeId(format!("Process:{entry_slug}-{hash}"))
}
```

---

## New crate: `cih-community`

**Location:** `crates/cih-community/`

### `Cargo.toml`

```toml
[package]
name = "cih-community"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
cih-core.workspace = true
anyhow.workspace = true
serde.workspace = true
serde_json.workspace = true
blake3.workspace = true
petgraph = "0.6"
```

Add `petgraph = "0.6"` to `[workspace.dependencies]` in the root `Cargo.toml`.
Add `"crates/cih-community"` to `[workspace.members]`.

### File structure

```
crates/cih-community/src/
  lib.rs           — public API: CommunityConfig, ProcessConfig, detect_communities, trace_processes
  graph.rs         — build petgraph UnGraph / DiGraph from Node/Edge slices
  leiden.rs        — Louvain/Leiden modularity clustering (phases 1 + 3)
  prng.rs          — Mulberry32 seeded PRNG (seed 0xc0de)
  label.rs         — heuristic community label from member file paths
  cohesion.rs      — internal edge density scoring (sampled)
  entry_points.rs  — entry point scoring (callee/caller ratio × name multipliers)
  bfs.rs           — BFS process trace collection + two-pass deduplication
```

### Public API (`lib.rs`)

```rust
pub struct CommunityConfig {
    pub resolution: f64,              // 1.0 normal; 2.0 large graphs
    pub max_iterations: u32,          // 0 = unlimited; 3 for large graphs
    pub seed: u32,                    // 0xc0de — fixed for reproducibility
    pub large_graph_threshold: usize, // 10_001
    pub min_confidence_large: f32,    // 0.5 — edge filter on large graphs
    pub min_community_size: usize,    // 2 — singletons are discarded
}
impl Default for CommunityConfig { /* constants above */ }

pub struct ProcessConfig {
    pub max_trace_depth: usize,       // 10
    pub max_branching: usize,         // 4
    pub max_processes: usize,         // dynamic — use for_symbol_count()
    pub min_steps: usize,             // 3 (traces shorter than 3 discarded)
    pub min_trace_confidence: f32,    // 0.5
}
impl ProcessConfig {
    /// Dynamic max: max(20, min(300, symbol_count / 10))
    pub fn for_symbol_count(symbol_count: usize) -> Self { … }
}

pub struct CommunityOutput {
    pub nodes: Vec<Node>,              // NodeKind::Community
    pub edges: Vec<Edge>,              // EdgeKind::MemberOf (symbol → community)
    pub memberships: Vec<(NodeId, NodeId)>, // (symbol_id, community_id) — input to trace_processes
}

pub struct ProcessOutput {
    pub nodes: Vec<Node>,              // NodeKind::Process
    pub edges: Vec<Edge>,              // EdgeKind::StepInProcess
}

pub fn detect_communities(nodes: &[Node], edges: &[Edge], cfg: &CommunityConfig) -> CommunityOutput;
pub fn trace_processes(
    nodes: &[Node],
    edges: &[Edge],
    memberships: &[(NodeId, NodeId)],
    cfg: &ProcessConfig,
) -> ProcessOutput;
```

---

## Algorithm implementations

### `prng.rs` — Mulberry32 seeded PRNG

Port directly from the TypeScript vendor. Gives Leiden deterministic node-visit ordering across runs.

```rust
pub struct Mulberry32 { state: u32 }

impl Mulberry32 {
    pub fn new(seed: u32) -> Self { Self { state: seed } }

    pub fn next_f64(&mut self) -> f64 {
        self.state = self.state.wrapping_add(0x6d2b79f5);
        let mut t = self.state ^ (self.state >> 15);
        t = t.wrapping_mul(1 | self.state);
        t = t ^ t.wrapping_add(t.wrapping_mul(61 | t));
        t = t ^ (t >> 14);
        (t as f64) / 4_294_967_296.0
    }

    pub fn shuffle<T>(&mut self, v: &mut Vec<T>) { /* Fisher-Yates using next_f64 */ }
}
```

---

### `graph.rs` — petgraph construction

**For community detection** (undirected):
```rust
pub fn build_community_graph(
    nodes: &[Node],
    edges: &[Edge],
    large: bool,
    min_confidence: f32,
) -> (UnGraph<NodeId, f32>, HashMap<NodeId, NodeIndex>)
```
- Include node kinds: `Class`, `Interface`, `Method`, `Constructor`.
- Include edge kinds: `Calls`, `Extends`, `Implements`. Exclude self-loops.
- If `large`: drop edges with `confidence < min_confidence`; after construction, remove nodes with
  degree ≤ 1.
- Each filtered edge is inserted as undirected.

**For process BFS** (directed, weighted):
```rust
pub fn build_calls_digraph(
    nodes: &[Node],
    edges: &[Edge],
    min_confidence: f32,
) -> (DiGraph<NodeId, f32>, HashMap<NodeId, NodeIndex>)
```
- Include only `Calls` edges with `confidence ≥ min_confidence`. Directed src → dst.

**Large-graph detection:**
```rust
pub fn is_large_graph(nodes: &[Node]) -> bool {
    nodes.iter().filter(|n| matches!(
        n.kind, NodeKind::Class | NodeKind::Interface | NodeKind::Method | NodeKind::Constructor
    )).count() > 10_000
}
```

---

### `leiden.rs` — Louvain community detection

Implements Louvain phases 1 (local moving) + 3 (aggregation) — equivalent to Leiden for practical
codebases. Uses `Mulberry32` for node-visit ordering.

**Data structures:**
```rust
struct Partition {
    node_to_comm: Vec<usize>,
    comm_to_nodes: HashMap<usize, Vec<usize>>,
    comm_weights: Vec<f64>,   // sum of internal edge weights per community
    node_weights: Vec<f64>,   // sum of incident edge weights per node
    total_weight: f64,
}
```

**Signature:**
```rust
pub fn louvain(
    graph: &UnGraph<NodeId, f32>,
    resolution: f64,
    max_iterations: u32,
    rng: &mut Mulberry32,
) -> Vec<usize>  // node_index → community_index
```

**Phase 1 — Local moving** (repeat until stable or `max_iterations` reached):
```
node_order = all node indices, shuffled with rng
for node i in node_order:
    remove i from its current community (update comm_weights)
    for each neighbor j:
        gain = ΔQ(i, comm[j], resolution, partition)
        track best_gain, best_comm
    assign i to best_comm (update comm_weights)
```

**Modularity gain formula:**
```
ΔQ = w_i_c / m  −  resolution × (k_i × Σk_c) / (2m²)
```
where `w_i_c` = edge weight sum from node i to community c; `m` = total edge weight;
`k_i` = degree of i; `Σk_c` = sum of degrees in c; `resolution` = 1.0 or 2.0.

**Phase 3 — Aggregation:** collapse communities into super-nodes; new graph has super-node edges =
sum of inter-community edge weights; recurse.

**Timeout:** enforce 60-second wall-clock limit via `std::time::Instant`. On timeout, return
single-community partition (all nodes in community 0) — graceful degradation, not a panic.

**Output:** renumber communities 0..N sorted by descending member count (Community:0 = largest).

---

### `label.rs` — Heuristic community labels

```rust
pub fn heuristic_label(member_file_paths: &[&str], comm_idx: usize) -> String
```

Three-tier fallback:
1. **Folder-based:** take the last directory component of each file path. Skip generic tokens:
   `["src","lib","core","utils","common","shared","helpers","java","main","kotlin","resources","test"]`.
   Find the mode (most frequent non-generic dir). Capitalize first letter.
2. **Name-prefix:** if 3+ members share a name prefix longer than 2 chars, use that prefix.
   Capitalize.
3. **Fallback:** `Cluster_{comm_idx}`.

---

### `cohesion.rs` — Community cohesion (sampled)

```rust
pub fn cohesion_score(
    members: &[NodeIndex],
    graph: &UnGraph<NodeId, f32>,
    sample_size: usize,   // 50
) -> f64
```

```
sample = &members[..min(sample_size, members.len())]
for node in sample:
    for neighbor of node in graph:
        total += 1
        if neighbor ∈ members_set: internal += 1
return (internal / total).clamp(0.0, 1.0)
```

---

### `entry_points.rs` — Entry point scoring

Score all `Method`/`Constructor` nodes in the directed calls graph; return top 200 sorted descending.

```rust
pub fn score_entry_points(
    nodes: &[Node],
    digraph: &DiGraph<NodeId, f32>,
    node_index: &HashMap<NodeId, NodeIndex>,
) -> Vec<(NodeId, f64)>
```

```
base_score = callee_count / (caller_count + 1)
final_score = base_score × name_multiplier(node.name)
```

**Java entry-point patterns (multiplier 1.5):**
- Starts with: `main`, `init`, `execute`, `run`, `start`, `handle`, `process`, `perform`,
  `dispatch`, `trigger`, `fire`, `emit` (followed by uppercase or `_`)
- Ends with: `Handler`, `Controller`, `Listener`, `Endpoint`
- Exact: `main`

**Utility patterns (multiplier 0.3):**
- Starts with: `get`, `set`, `is`, `has`, `to`, `from`, `format`, `parse`, `validate`, `convert`,
  `log`, `debug`
- Ends with: `Helper`, `Util`, `Utils`

**Default:** multiplier 1.0.

---

### `bfs.rs` — Process BFS tracing

```rust
pub fn trace_processes(
    digraph: &DiGraph<NodeId, f32>,
    entry_points: &[(NodeId, f64)],
    memberships: &HashMap<NodeId, NodeId>,  // symbol → community_id
    cfg: &ProcessConfig,
) -> Vec<Vec<NodeIndex>>
```

**BFS per entry point:**
```
queue: VecDeque<(NodeIndex, Vec<NodeIndex>)>  // (current_node, path_so_far)
push (entry_idx, [entry_idx])
while queue not empty:
    (cur, path) = pop_front
    callees = digraph.neighbors(cur) where edge.weight >= min_trace_confidence
    if callees empty or path.len() >= max_trace_depth:
        if path.len() >= min_steps: record path as trace
        continue
    for callee in callees.filter(|n| !path.contains(n)).take(max_branching):
        push (callee, path + [callee])
cap: save at most max_branching × 3 traces per entry point
```

**Deduplication — two passes:**

Pass 1 — Subset removal:
```
sort all_traces by length descending
for trace (encoded as "id1->id2->id3"):
    if any retained trace contains this string as a substring: discard
    else: keep
```

Pass 2 — Endpoint deduplication:
```
key = (trace.first, trace.last)
for each key: keep only the longest trace
```

After dedup, apply `cfg.max_processes` cap (take first N by step count descending).

---

## Node and edge construction (`lib.rs`)

### Community nodes

```rust
Node {
    id: community_id(sorted_idx),          // "Community:0", "Community:1", …
    kind: NodeKind::Community,
    name: heuristic_label,
    qualified_name: heuristic_label.clone(),
    file: None,
    range: None,
    props: Some(json!({
        "label":          label,
        "heuristic_label": label,
        "cohesion":       cohesion_score,
        "symbol_count":   member_count,
        "color":          COLOR_PALETTE[idx % 12],
    })),
}
```

Color palette (12-color cycle):
```
["#ef4444","#f97316","#eab308","#22c55e","#06b6d4","#3b82f6",
 "#8b5cf6","#d946ef","#ec4899","#f43f5e","#14b8a6","#84cc16"]
```

Sort communities by descending member count before assigning indices so Community:0 is always the
largest cluster.

### `MEMBER_OF` edges

One edge per symbol in the community:
```rust
Edge {
    src: symbol_node_id,
    dst: community_id(idx),
    kind: EdgeKind::MemberOf,
    confidence: 1.0,
    reason: "leiden".into(),
}
```

### Process nodes

```rust
let entry_slug = entry_method_name.to_lowercase().replace([':', '#', '/'], "-");
let trace_hash  = &blake3_hex(trace_node_ids_joined)[..6];

Node {
    id: process_id(&entry_slug, trace_hash),  // "Process:handle-login-a3f9c1"
    kind: NodeKind::Process,
    name: format!("{} → {}", entry_name, terminal_name),
    qualified_name: …,
    file: None,
    range: None,
    props: Some(json!({
        "label":           label,
        "process_type":    if cross_community { "cross_community" } else { "intra_community" },
        "step_count":      trace.len(),
        "communities":     community_ids_vec,
        "entry_point_id":  entry_id,
        "terminal_id":     terminal_id,
    })),
}
```

### `STEP_IN_PROCESS` edges

One edge per step in the trace (1-indexed; step number encoded in `reason`):
```rust
Edge {
    src: trace[step_idx],           // symbol_node_id at this step
    dst: process_id,
    kind: EdgeKind::StepInProcess,
    confidence: 1.0,
    reason: format!("step:{}", step_idx + 1),
}
```

---

## Engine changes: `cih-engine`

### `cih-engine/Cargo.toml`

Add `cih-community.workspace = true`.

### New `Discover` subcommand

```rust
/// Run community detection + process tracing on an already-analyzed repo.
Discover {
    repo: PathBuf,
    #[arg(long, env = "FALKOR_URL")]
    falkor_url: Option<String>,
    #[arg(long, env = "CIH_GRAPH_KEY")]
    graph_key: Option<String>,
    #[arg(long)]
    no_load: bool,
    #[arg(long)]
    json: bool,
},
```

### `run_discover(repo, falkor_url, graph_key, no_load, json)`

```
1. Glob .cih/artifacts/*/nodes.jsonl — pick the entry with the newest mtime.
2. nodes = GraphArtifacts::read_nodes(path)?
3. edges = GraphArtifacts::read_edges(path)?
4. large = is_large_graph(&nodes)
5. comm_cfg = if large { CommunityConfig { resolution: 2.0, max_iterations: 3, .. } }
              else     { CommunityConfig::default() }
6. comm_out = detect_communities(&nodes, &edges, &comm_cfg)
7. symbol_count = nodes with kind ∈ {Method, Constructor, Class, Interface}
8. proc_cfg = ProcessConfig::for_symbol_count(symbol_count)
9. proc_out = trace_processes(&nodes, &edges, &comm_out.memberships, &proc_cfg)
10. all_nodes = comm_out.nodes + proc_out.nodes
    all_edges = comm_out.edges + proc_out.edges
11. version = first 16 hex chars of blake3(all node ids + edge srcs/dsts sorted)
    write to .cih/artifacts-community/<version>/nodes.jsonl + edges.jsonl
12. if !no_load: load_to_falkor(url, graph_key, &artifacts)
13. print/json summary
```

**Output directory:** `.cih/artifacts-community/<version>/` — kept separate from
`.cih/artifacts/<version>/` so `analyze` and `discover` can be re-run independently without
invalidating each other.

---

## FalkorDB changes: `cih-falkor`

### Update `context()` (`cih-falkor/src/lib.rs`)

Replace the `processes: vec![]` placeholder (search for the TODO comment near `SymbolContext`
construction):

```rust
// Query all Process nodes that include this symbol as a step.
let proc_query = "MATCH (s:Symbol {id: $id})-[:STEP_IN_PROCESS]->(p:Symbol) \
                  WHERE p.kind = 'Process' RETURN p.id";
let processes = self.query_params(proc_query, &[("id", id.as_str())]).await?
    .into_iter()
    .filter_map(|row| row.into_iter().next())
    .collect();
```

Use the existing `query_params` helper (or the `rows` + inline-escape pattern already used elsewhere
in the file — mirror whichever pattern is used for similar `MATCH` queries in `cih-falkor`).

---

## MCP server changes: `cih-server`

### New `communities` tool (`src/main.rs`)

```rust
#[tool(description = "List community clusters detected in the codebase.")]
async fn communities(
    &self,
    #[tool(param)] args: CommunitiesArgs,
) -> Result<CallToolResult, McpError>
```

Query: `MATCH (c:Symbol) WHERE c.kind = 'Community' RETURN c.id, c.name, c.symbolCount, c.cohesion`

Return type `Vec<CommunityInfo>`:
```rust
pub struct CommunityInfo {
    pub id:           String,
    pub name:         String,
    pub symbol_count: u64,
    pub cohesion:     f64,
}
```

The `context` tool automatically returns populated `processes` once FalkorDB is updated — no
additional changes to `cih-server`.

---

## Tests (8 required)

### `cih-community` unit tests

1. **`community_detection_splits_two_cliques`** — 4 Class nodes forming two triangle cliques
   connected by one weak edge → Leiden produces exactly 2 non-singleton communities.

2. **`seeded_rng_is_deterministic`** — run `detect_communities` twice on the same input with the
   same seed (0xc0de) → identical `CommunityOutput.edges` (same MEMBER_OF assignments).

3. **`singleton_communities_are_discarded`** — one isolated Class node produces zero Community
   nodes (filtered by `min_community_size = 2`).

4. **`process_trace_min_steps_enforced`** — graph A→B (length 2) produces no process;
   A→B→C (length 3) produces exactly one process.

5. **`process_cycle_prevention`** — graph A→B→C→A; BFS from A terminates and produces a finite
   number of traces (no infinite loop).

6. **`process_cross_community`** — trace spanning nodes in two different community memberships
   gets `process_type = "cross_community"` in its props.

7. **`process_dedup_keeps_longest`** — two paths A→B→C and A→B→C→D from the same entry; only
   the 4-step trace survives after deduplication.

### `cih-engine` integration test

8. **`discover_emits_community_and_process_artifacts`** — use the existing `temp_repo()` /
   `analyze_emit` pattern (at least 2 Java files with a `CALLS` edge, as proven in Phase 4 tests).
   Call `run_discover_core(&scan, &[])` (a thin wrapper over `run_discover` that skips FalkorDB
   loading). Assert that `.cih/artifacts-community/<v>/nodes.jsonl` exists and contains at least
   one node with `kind = "Community"`.

---

## Sequencing (implement in this order)

1. **`cih-core`** — add `community_id()` and `process_id()` helpers.
   `cargo test -p cih-core` must stay green.

2. **`cih-community` crate** — scaffold, then implement modules in order:
   `prng.rs` → `graph.rs` → `leiden.rs` → `label.rs` → `cohesion.rs` → `entry_points.rs` →
   `bfs.rs` → `lib.rs`. Add unit tests as you go (tests 1–7 above).

3. **`cih-engine` Discover subcommand** — wire `cih-community` into the CLI and add the
   integration test (test 8).

4. **`cih-falkor` `context()` update** — populate `processes`. Can be verified with a
   hand-seeded FalkorDB graph or via the existing `cih-falkor` test harness.

5. **`cih-server` `communities` tool** — add last; lowest risk, most independent.

6. **`ROADMAP.md`** — mark Phase 5 ✅ with verified date and test count.

---

## Verification (end-to-end)

```bash
# 1. Analyze a Java repo (Phase 4 must have run first)
cargo run -p cih-engine -- analyze <java-repo> --all --no-load

# 2. Run community detection + process tracing
cargo run -p cih-engine -- discover <java-repo> --no-load

# 3. Inspect output
cat <java-repo>/.cih/artifacts-community/*/nodes.jsonl | jq 'select(.kind=="Community")'
cat <java-repo>/.cih/artifacts-community/*/nodes.jsonl | jq 'select(.kind=="Process")'

# 4. Load to FalkorDB and verify via MCP
FALKOR_URL=redis://127.0.0.1:6380 cargo run -p cih-engine -- discover <java-repo>
# MCP Inspector → context("Method:com.example.AuthController#login/1")
#   → processes field should be non-empty
# MCP Inspector → communities
#   → list of cluster names with cohesion scores

# 5. All tests green
cargo test --workspace
cargo clippy --workspace
```

---

## Critical files summary

| Action | File |
|--------|------|
| **Create** | `crates/cih-community/Cargo.toml` |
| **Create** | `crates/cih-community/src/lib.rs` |
| **Create** | `crates/cih-community/src/graph.rs` |
| **Create** | `crates/cih-community/src/leiden.rs` |
| **Create** | `crates/cih-community/src/prng.rs` |
| **Create** | `crates/cih-community/src/label.rs` |
| **Create** | `crates/cih-community/src/cohesion.rs` |
| **Create** | `crates/cih-community/src/entry_points.rs` |
| **Create** | `crates/cih-community/src/bfs.rs` |
| **Edit** | `Cargo.toml` — add `cih-community` to `[workspace.members]` + `petgraph = "0.6"` to `[workspace.dependencies]` |
| **Edit** | `crates/cih-core/src/lib.rs` — add `community_id()` + `process_id()` |
| **Edit** | `crates/cih-engine/Cargo.toml` — add `cih-community.workspace = true` |
| **Edit** | `crates/cih-engine/src/main.rs` — add `Discover` subcommand + `run_discover` |
| **Edit** | `crates/cih-falkor/src/lib.rs` — populate `processes` in `context()` |
| **Edit** | `crates/cih-server/src/main.rs` — add `communities` MCP tool |
| **Edit** | `ROADMAP.md` — mark Phase 5 ✅ when done |

---

## Risks / decisions

- **Leiden vs full Leiden.** The refinement sub-phase is a theoretical convergence guarantee, not a
  practical quality difference for typical codebases (~5–100k nodes). Louvain phases 1+3 match
  the reference TypeScript exactly.
- **Mulberry32 seed.** Fixed at `0xc0de` — same as GitNexus — for reproducible clustering. Never
  change this; doing so invalidates all existing Community node ids.
- **Timeout graceful degradation.** If Louvain exceeds 60 s (e.g., pathologically dense graph),
  return a single community rather than panicking. Log a warning with node/edge counts.
- **Community ids are index-stable.** Sorted by descending member count before id assignment.
  If the graph changes, ids shift — that is expected; a full re-discover re-loads via MERGE.
- **Process hash length.** The 6-hex suffix from blake3 gives 16M combinations per entry slug.
  Collision probability per repo is negligible; promote to 8 chars only if collisions appear.
- **DI-aware process routing** (interface→`@Service` impl) is Phase 13. Phase 5 traces follow the
  declared call edges as-is; Spring wiring is not yet resolved.
