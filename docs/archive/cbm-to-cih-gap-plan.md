# CBM → CIH Gap Plan (revised)

> **Why:** After analyzing codebase-memory-mcp we identified 5 capabilities worth adding to CIH for large Java/Spring enterprise codebases. This revision corrects 7 design bugs found in the first draft.

---

## Dependency order

```
Gap 4 (Constant Propagation) → Gap 3 (Call-site Records)
                                          ↓
                              enriches trace_flow output
Gap 1 (Complexity Metrics)   — independent, parse-phase addition
Gap 2 (MinHash SIMILAR_TO)   — independent, post-resolve phase
Gap 5 (Artifact Sharing)     — fully orthogonal
```

**Implement in order: 4 → 3 → 1 → 2 → 5**

---

## Gap 4: Constant Propagation — language-provider service

*Resolves `static final String` constants to their literal values at extraction time. Enriches call-site args (Gap 3), SQL/JPQL extraction (already working), and annotation parameters.*

**Crates:** `cih-core`, `cih-lang` (trait + java/), `cih-resolve`

### Design constraint (was P1)
The original design only handled simple identifiers in the same class. Java constants are also qualified (`Config.BASE_URL`), statically imported (`import static ...BASE_URL`), or inherited. Annotation fields (`@FeignClient(url = BASE_URL)`) are annotation data, not call arguments — they flow through the annotation extractor, not Gap 3. The verification test must reflect this.

### cih-core — add to ParsedFile (`ir.rs`)
```rust
pub struct StringConstant {
    pub const_name: String,
    pub owner_fqcn: String,
    pub value: String,      // folded literal value
    pub dynamic: bool,      // true when concat included non-literals
    pub range: Range,
}
// add to ParsedFile:
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub string_constants: Vec<StringConstant>,
```

### cih-lang — define a ConstantResolver trait (new `constant_resolver.rs`)
```rust
pub struct ResolutionContext<'a> {
    pub file: &'a Path,
    pub owner_fqcn: &'a str,
    pub imports: &'a [ImportRecord],
}

/// Resolves a name (simple identifier or Qualified.IDENT) to its folded
/// string literal value, using the constant index built from ParsedFiles.
pub trait ConstantResolver: Send + Sync {
    fn resolve(&self, name: &str, ctx: &ResolutionContext<'_>) -> Option<String>;
}
```

Implement `JavaConstantResolver` in `cih-lang/src/java/`:
1. **Simple identifier in same class** — look up `(owner_fqcn, name)` in index
2. **Qualified `Cls.NAME`** — strip prefix, look up `(resolved_fqcn, NAME)` via imports index
3. **Static import** — parse `import static com.example.Config.NAME`, look up `(Config_fqcn, NAME)`
4. **Inherited** — walk superclass chain (one level, indexed set only)

### cih-lang/java/parse.rs
- The existing `fold_string_init` already handles literal folding and string concat.
- Rename `collect_sql_constants_in` → `collect_static_string_constants`. Apply to **all** `static final String` fields, not just SCREAMING_CASE SQL names. Populate `builder.string_constants`.

### cih-resolve — build the index and expose the resolver
```rust
pub fn build_java_constant_resolver(parsed: &[ParsedFile]) -> impl ConstantResolver
```
The resolver is injected into the resolve pipeline for use by Gap 3's call-site extractor, the existing SQL extractor, and the annotation extractor.

**Consumers:**
- Gap 3: call-site arg resolution
- `cih-resolve/src/db_access.rs`: already uses folded SQL — replace ad-hoc map with this resolver
- Annotation extractor (future): reads `@FeignClient(url = BASE_URL)`

**No new MCP tools.** Payoff flows through `trace_flow` once Gap 3 is wired.

---

## Gap 3: Call-site Records in CALLS Edges

*Capture argument expressions at each call site. Multiple calls from X to Y (different lines) collapse into one edge by (src, dst, kind) — so argument data must be stored as a list of call-site records, not a single list of args.*

**Crates:** `cih-core`, `cih-lang/java`, `cih-resolve`, `cih-engine`, `cih-falkor`, `cih-graph-store`, `cih-server`

### Design constraint: edge deduplication (was P1)
`analyze.rs:1081` `combined_edges()` keeps the highest-confidence edge per `(src, dst, kind)`. Multiple calls to the same target collapse into one edge. A single `args: Vec<String>` would lose all but one call site. The fix: store `Vec<CallSiteRecord>` and **merge** when deduplicating.

### cih-core — new type + ReferenceSite field
```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallSiteRecord {
    pub range: Range,
    pub args: Vec<String>,  // resolved (constant-propagated) arg texts, <= 120 chars each
}

// add to ReferenceSite:
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub arg_texts: Vec<String>,      // raw arg texts captured at parse time

// add to Edge:
#[serde(default, skip_serializing_if = "Option::is_none")]
pub props: Option<serde_json::Value>,
// CALLS edges use: props = Some(json!({ "call_sites": [{ "range": ..., "args": [...] }] }))
// capped at 20 call-site records per edge
```

### cih-engine/src/analyze.rs — merge call_sites on deduplication
In `combined_edges`, when two CALLS edges with the same `(src, dst, CALLS)` are merged:
```rust
// Instead of dropping the lower-confidence edge, merge its call_sites
fn merge_call_sites(winner: &mut Edge, loser: &Edge) {
    let Some(loser_cs) = call_sites_from(loser) else { return };
    let entry = winner.props.get_or_insert_with(|| json!({"call_sites": []}));
    let existing = entry["call_sites"].as_array_mut().unwrap();
    existing.extend(loser_cs);
    existing.truncate(20);
}
```

### cih-lang/java/parse.rs
In `reference_site()`, when `kind == RefKind::Call`, walk `arguments` AST node named children (skip comments), truncate each text at 120 chars, store in `site.arg_texts`.

### cih-resolve/src/common/emit.rs
After resolving a CALLS edge, apply the `ConstantResolver`: for each arg in `site.arg_texts`, attempt resolution. Build a `CallSiteRecord { range: site.at_range, args: resolved_args }`. Set `edge.props = Some(json!({ "call_sites": [record] }))`.

### cih-falkor/src/lib.rs
`edges_to_list()`: serialize `call_sites` from `e.props` as a string column.
In the MERGE: `SET r.callSites = row.callSites`.

### FlowNode → FlowHop redesign (was P1)
`FlowNode` represents a symbol, not a traversal step. The flow query (`flow_downstream`/`flow_upstream`, falkor:593) collapses paths to shortest-parent and returns no edge properties. Attaching `args` to `FlowNode` models relationship data on the wrong object.

Replace with a two-type model in `cih-graph-store/src/lib.rs`:
```rust
/// One step in a trace_flow result: the symbol reached, and the edge used to reach it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlowHop {
    pub node: FlowNode,
    pub via: Option<FlowEdge>,   // None for the root entry point
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlowEdge {
    pub kind: String,            // "CALLS", "HANDLES_ROUTE", etc.
    pub call_sites: Vec<CallSiteArgs>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallSiteArgs {
    pub args: Vec<String>,
}
```

Update `flow_downstream` / `flow_upstream` Cypher to return edge kind and `r.callSites` per hop. Variable-length path queries don't return per-edge properties in FalkorDB; use a two-phase approach: (1) BFS to get node order and parent relationships (existing query), (2) for each adjacent (parent, child) pair in the result, fetch `r.callSites` from the CALLS edge between them.

Update `cih-server` `trace_flow` tool to serialize `Vec<FlowHop>` instead of `Vec<FlowNode>`.

---

## Gap 1: Complexity Metrics

*Per-method cyclomatic + cognitive + loop depth; transitive loop depth propagated along CALLS edges with the correct additive formula from CBM.*

**Crates:** `cih-core`, `cih-lang/java`, `cih-resolve` (new pass), `cih-falkor`, `cih-server`

### Design constraints (was P1 + P2)

**Transitive formula:** The draft used `max(own, callee)`, which leaves two depth-1 callers nesting as depth 1. CBM `pass_complexity.c:102` uses the additive formula:
```
tld(id) = loop_depth(id) + max over CALLS-callees of tld(callee)
```
Back-edges (cycles) set `is_recursive = true` and return 0 from the DFS to avoid infinite inflation. This is an upper-bound candidate signal, not a guarantee — stdlib calls (`Collections.sort`, `stream().forEach()`) that internally loop will inflate scores.

**Storage:** Node.props is serialized as a single JSON string in FalkorDB; only the fixed promoted set in `nodes_to_list` (falkor:835) is queryable as graph properties. Complexity metrics must be added as explicit promoted fields — not buried in props — or `n.cyclomatic >= $min_cc` will not work.

**Language neutrality (was P2):** Adding `cyclomatic: u16` directly to `SymbolDef` conflates "zero complexity" with "language doesn't support this analysis." Use an optional language-tagged record instead.

### cih-core — language-neutral optional record
```rust
/// Optional complexity analysis. None = language provider did not compute this.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub complexity: Option<ComplexityRecord>,

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComplexityRecord {
    pub provider: String,   // e.g., "java"
    pub cyclomatic: u16,
    pub cognitive: u16,
    pub loop_depth: u8,
    pub is_recursive: bool, // set during transitive pass
}
```
`transitive_loop_depth` is written as a promoted field `Node.props["transitiveLoopDepth"]` (u8) after the propagation pass — separate from `ComplexityRecord` — so it gets a dedicated graph property column.

### cih-lang/java/parse.rs
Add `fn compute_complexity(body: TsNode<'_>) -> ComplexityRecord`:
- **Cyclomatic:** +1 per `if_statement`, else-if clause, `case` in switch, `while_statement`, `for_statement`, `enhanced_for_statement`, `do_statement`, `catch_clause`, `conditional_expression`, `&&`/`||` operators.
- **Cognitive (Sonar model):** penalty scales with syntactic nesting depth; `if`/`for`/`while`/`do`/`try`/`switch` increment nesting; `else` adds 1 flat; labelled `break`/`continue` add 1.
- **loop_depth:** max depth of nested loop AST nodes.
- **provider:** `"java"`.

Call in `collect_method` and `collect_constructor`. Set `sym.complexity = Some(record)`.
Write `cyclomatic`, `cognitive`, `loopDepth` into `Node.props` so they reach the falkor bulk loader.

### cih-resolve/src/complexity.rs (new file)
```rust
/// Additive DFS matching CBM pass_complexity.c:102.
/// tld(id) = loop_depth(id) + max_over_callees(tld(callee))
/// Back-edges: set is_recursive=true, return 0 (break the cycle).
/// Cap tld at 20 to prevent runaway values through stdlib.
/// Mutates Node.props["transitiveLoopDepth"] and marks is_recursive in-place.
pub fn propagate_loop_depths(nodes: &mut [Node], edges: &[Edge])
```

Called from `cih-engine/src/analyze.rs` after the resolve phase, before writing artifacts.

### cih-falkor/src/lib.rs — explicit promoted fields
Extend `nodes_to_list` to promote alongside existing fields:
```rust
let cyclomatic    = cnum_u64(prop_u64(n, "cyclomatic"));
let cognitive     = cnum_u64(prop_u64(n, "cognitive"));
let loop_depth    = cnum_u64(prop_u64(n, "loopDepth"));
let transitive_ld = cnum_u64(prop_u64(n, "transitiveLoopDepth"));
```
Add to the node format string and to the bulk-load MERGE `SET` clause. These become first-class graph properties queryable as `n.cyclomatic`, `n.transitiveLoopDepth`, etc. — no `toInteger()` needed.

### New MCP tool: `complexity_hotspots`
```rust
async fn complexity_hotspots(
    &self,
    min_cyclomatic: Option<u16>,
    min_cognitive: Option<u16>,
    min_transitive_loop: Option<u8>,
    limit: usize,
) -> Result<Vec<HotspotNode>>;
```
Cypher:
```cypher
MATCH (n:Symbol) WHERE n.kind IN ['Method','Constructor']
  AND n.transitiveLoopDepth >= $tl
RETURN n.id, n.name, n.file, n.cyclomatic, n.cognitive, n.transitiveLoopDepth
ORDER BY n.transitiveLoopDepth DESC, n.cyclomatic DESC LIMIT $limit
```

---

## Gap 2: MinHash + SIMILAR_TO Edges

*Fingerprint each method body with AST token trigrams, K=64 MinHash; 32×2 LSH for near-clone detection at threshold 0.95 (near-exact duplicates only).*

**Crates:** `cih-core`, `cih-lang/java`, `cih-resolve` (new file), `cih-falkor`, `cih-graph-store`, `cih-server`

### Design constraints (was P2 × 2)

**CBM parameters** (from `minhash.h`: K=64, threshold=0.95, min 30 leaf tokens, 32 bands × 2 rows, max 10 edges per node). The draft used K=128, threshold=0.65, 16×8 LSH. At Jaccard=0.65 with 16×8 LSH, recall is only ~40% (band prob = 0.65^8 = 0.032; P(candidate) = 1 − 0.968^16 ≈ 0.40). At 0.95 with 32×2, recall exceeds 99%.

**Language neutrality (was P2):** Body fingerprinting is Java-only in phase 1. Use an optional language-tagged record (same pattern as `ComplexityRecord`).

### cih-core
```rust
// add to EdgeKind:
SimilarTo,  // cypher_label = "SIMILAR_TO"

/// Optional MinHash fingerprint for near-clone detection.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub body_fingerprint: Option<BodyFingerprint>,

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BodyFingerprint {
    pub provider: String,       // e.g., "java"
    pub leaf_token_count: u32,  // leaf AST node count; size gate
    pub minhash: [u32; 64],     // K=64 MinHash values
}
```

### cih-lang/java/parse.rs
In `collect_method` / `collect_constructor`: walk body AST, count leaf nodes, normalize leaf types (identifier→`"I"`, string→`"S"`, number→`"N"`, type→`"T"`, keyword→`"K"`), build trigrams of normalized types, compute K=64 MinHash with compile-time fixed seed array. If `leaf_token_count < 30`, leave `body_fingerprint = None`.

### cih-resolve/src/similarity.rs (new file)

Parameters matching CBM minhash.h:
```rust
const K: usize = 64;
const JACCARD_THRESHOLD: f32 = 0.95;
const LSH_BANDS: usize = 32;
const LSH_ROWS: usize = 2;            // threshold approx. (1/32)^(1/2) ≈ 0.18
const MAX_EDGES_PER_NODE: usize = 10;
const MIN_LEAF_TOKENS: u32 = 30;

const SEEDS: [u64; 64] = [/* compile-time fixed array */]; // deterministic

pub fn emit_similar_to_edges(nodes: &[Node]) -> Vec<Edge>
// 1. Collect nodes with body_fingerprint where leaf_token_count >= MIN_LEAF_TOKENS
// 2. Group by provider (language) — no cross-language pairs
// 3. LSH: hash each band of LSH_ROWS consecutive values; bucket by band_hash → candidates
// 4. For each candidate pair: exact Jaccard = count(a[i]==b[i]) / K
// 5. Emit SIMILAR_TO if Jaccard >= JACCARD_THRESHOLD; confidence = jaccard score
// 6. Cap at MAX_EDGES_PER_NODE per source node (sort by descending Jaccard before cap)
```

### cih-engine/src/analyze.rs
After resolve phase: call `emit_similar_to_edges(&nodes)`, append to edges artifact.

### cih-graph-store — add
```rust
async fn similar_methods(&self, id: &NodeId, min_jaccard: f32, limit: usize) -> Result<Vec<SimilarMethod>>;
pub struct SimilarMethod { pub id: NodeId, pub name: String, pub file: String, pub jaccard: f32 }
```

### New MCP tool: `find_duplicates(name, min_jaccard?, limit?)`
Calls `similar_methods`. Returns near-duplicate candidates with Jaccard score and file path.

---

## Gap 5: Team Artifact Sharing

*Compress the full CIH state into `.cih/graph.db.zst` for git-committable team snapshots. Bootstrap restores all incremental state (not just the graph) so subsequent `analyze` runs are incremental, not full re-indexes.*

**Crates:** `cih-core`, `cih-engine`. Add `zstd` to workspace dependencies.

### Design constraint: bootstrap requires more than nodes/edges (was P1)
Incremental re-indexing depends on `file-hashes.json`, `scope.json`, `repo-map.json`, and the `artifacts-community/` tree — not only `nodes.jsonl` and `edges.jsonl`. After a bare graph import, `analyze` would re-parse all files from scratch. The bootstrap command must restore the full incremental state. Registry metadata (`~/.cih/registry.json`) must also be registered.

### Bundle format: versioned manifest + all incremental state
```rust
// cih-core/src/artifacts.rs
pub struct CihBundleManifest {
    pub bundle_version: u8,       // 1
    pub cih_version: String,
    pub repo_name: String,
    pub root_path: String,
    pub indexed_at: String,        // ISO 8601
    pub artifact_version: String,
    pub has_community: bool,
    pub file_count: usize,
}
```

Bundle contents (each entry: 4-byte length prefix + zstd-compressed blob):
```
magic[8]:  "CIHPACK1"
manifest.json
nodes.jsonl
edges.jsonl
community-nodes.jsonl    (only if has_community)
community-edges.jsonl    (only if has_community)
file-hashes.json         <- incremental state
scope.json               <- incremental state
repo-map.json            <- incremental state
```

### cih-core/src/artifacts.rs — new methods
```rust
impl GraphArtifacts {
    pub fn export_bundle(
        &self,
        community: Option<&GraphArtifacts>,
        file_hashes: &Path,
        scope_json: &Path,
        repo_map_json: &Path,
        dest: &Path,
    ) -> std::io::Result<CihBundleManifest>

    pub fn import_bundle(
        bundle: &Path,
        cih_dir: &Path,   // restores all files into .cih/ in-place
    ) -> std::io::Result<(GraphArtifacts, Option<GraphArtifacts>, CihBundleManifest)>
}
```

### cih-engine/src/main.rs — new `artifact` subcommand
```
cih-engine artifact export    <repo> [--out .cih/graph.db.zst]
cih-engine artifact import    <repo> --bundle .cih/graph.db.zst [--falkor-url] [--graph-key]
cih-engine artifact bootstrap <repo>    # import + FalkorDB bulk-load + registry registration
```

`bootstrap` steps:
1. `import_bundle` → restore all `.cih/` state
2. Bulk-load nodes + edges (+ community nodes/edges) into FalkorDB
3. Register repo in `~/.cih/registry.json`
4. Subsequent `analyze` reads restored `file-hashes.json` → processes only changed files (incremental)

### cih-engine/src/analyze.rs — auto-bootstrap on cold start
At start of `analyze_from_scope_with_options`: if `BootstrapMode::Auto` and FalkorDB is empty and `.cih/graph.db.zst` exists, call `bootstrap`. Opt-in flag to avoid unexpected behavior in CI.

**Git setup (doc only):** users add `.cih/graph.db.zst merge=ours` to their `.gitattributes`.

---

## Critical files to modify

| File | Gaps | Notes |
|------|------|-------|
| `cih-core/src/ir.rs` | 4, 3, 1, 2 | `StringConstant`; `CallSiteRecord`; `ComplexityRecord`, `BodyFingerprint` on `SymbolDef`; `arg_texts` on `ReferenceSite`; `props` on `Edge` |
| `cih-core/src/artifacts.rs` | 5 | `CihBundleManifest`, `export_bundle`, `import_bundle` |
| `cih-lang/src/constant_resolver.rs` *(new)* | 4 | `ConstantResolver` trait + `ResolutionContext` |
| `cih-lang/src/java/parse.rs` | 4, 3, 1, 2 | string constant extraction; arg text capture; `compute_complexity`; body fingerprint |
| `cih-lang/src/java/constant_resolver.rs` *(new)* | 4 | `JavaConstantResolver` — handles simple, qualified, static-import, inherited |
| `cih-resolve/src/common/emit.rs` | 3 | apply `ConstantResolver`; build `CallSiteRecord`; set `edge.props` |
| `cih-resolve/src/complexity.rs` *(new)* | 1 | additive transitive loop depth DFS; back-edge → `is_recursive`; cap at 20 |
| `cih-resolve/src/similarity.rs` *(new)* | 2 | MinHash + LSH (K=64, threshold=0.95, 32×2, max 10 edges/node) |
| `cih-engine/src/analyze.rs` | 1, 2, 3, 5 | wire new passes; `merge_call_sites` in `combined_edges`; auto-bootstrap |
| `cih-engine/src/main.rs` | 5 | `artifact` subcommand |
| `cih-falkor/src/lib.rs` | 1, 3 | promote `cyclomatic`/`cognitive`/`loopDepth`/`transitiveLoopDepth` columns; serialize `callSites` on edges; two-phase flow query for edge data |
| `cih-graph-store/src/lib.rs` | 1, 2, 3 | `FlowHop`/`FlowEdge` replacing `FlowNode.args`; `complexity_hotspots`; `similar_methods` |
| `cih-server/src/main.rs` | 1, 2, 3 | `complexity_hotspots`; `find_duplicates`; update `trace_flow` to `Vec<FlowHop>` |

---

## Verification

1. **Gap 4** — Java file with `static final String BASE_URL = "/api/v1"` and a method calling `restTemplate.getForObject(BASE_URL + "/path", String.class)`. The CALLS edge's `call_sites[0].args[0]` should be `"/api/v1/path"` (constant-folded), not `"BASE_URL"`.

2. **Gap 3** — `trace_flow` on a Spring controller method returns `Vec<FlowHop>` where each hop's `via.call_sites` lists argument expressions from that call site. A method that calls the same target twice (different args) yields one CALLS edge with two `call_sites` entries.

3. **Gap 1** — A utility with a `for` loop called by a service method that also loops: utility `loop_depth=1`, service `own_loop_depth=1`, service `transitiveLoopDepth=2` (additive, not max). `complexity_hotspots(min_transitive_loop=2)` surfaces the service method.

4. **Gap 2** — Two near-identical service methods in different classes, same logic, ≥ 30 leaf tokens, differing only in variable names. `find_duplicates` returns the pair with Jaccard ≥ 0.95.

5. **Gap 5** — Run `artifact export`, drop the FalkorDB graph, run `artifact bootstrap`. Verify: (a) `list_repos` shows the repo, (b) `file-hashes.json` is restored under `.cih/`, (c) subsequent `cih-engine analyze` is incremental (processes only files changed since the snapshot, not a full re-parse).

6. **Regression** — `cargo test --workspace` passes with all existing tests green.
