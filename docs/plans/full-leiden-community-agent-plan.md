# Plan: Full Leiden Communities, Agent Retrieval, and Community Documentation

## Summary

Replace CIH's current single-level Louvain-style local-move pass with a complete Leiden
implementation, retain CIH's confidence-weighted code graph and business metadata enrichment,
and make communities a measured retrieval aid for agents.

Community documentation will be generated under a dedicated top-level `communities/` branch.
Existing feature, role, route, and class documentation will move under a sibling `codebase/`
branch. Community membership is navigation context, not evidence: agent answers must continue to
cite concrete symbols, files, routes, and graph edges.

The change is successful when:

- Every emitted community is internally connected.
- Repeated discovery on an unchanged graph is deterministic.
- TypeScript and Python `Function` nodes participate in communities.
- Broad-question retrieval improves without regressing exact-symbol questions.
- The generated sidebar has separate **Codebase** and **Communities** branches.
- Fineract no longer produces community documentation dominated by tiny three-symbol pages.

## Current State and Problems

CIH currently calls `leiden::louvain()` from `cih-community`. The implementation performs
weighted local node moves and then stops. It does not implement Leiden refinement or aggregation,
so it cannot provide Leiden's connected-community guarantee or multi-level optimization.

Current strengths to preserve:

- Undirected graph construction from `CALLS`, `EXTENDS`, and `IMPLEMENTS` edges.
- Edge confidence used as weight; parallel relationships accumulate weight.
- Deterministic seed `0xc0de`.
- Low-confidence and degree-one filtering for large graphs.
- Community enrichment from routes, controllers, database tables, topics, stereotypes, and files.
- Process extraction after community detection.

Current gaps:

- `Function` nodes are excluded, weakening TypeScript and Python clustering.
- A timeout collapses every node into one community, which produces misleading output.
- Community IDs are ordinal and change when community size ordering changes.
- `AgentRunner` cannot directly inspect or expand a community.
- Wiki feature pages are derived from communities, but raw community topology has no separate
  documentation branch.
- The existing Fineract artifact has 2,432 communities, a median size of three symbols, 1,106
  communities of size three to five, and a largest community of 1,007 symbols.

## Full Leiden Implementation

### Dependency and adapter

- Add `leiden-rs` pinned to the reviewed `0.8.x` release line, with only the features needed for
  weighted undirected graph input.
- Record the exact resolved version and license in `Cargo.lock` and the dependency audit notes.
- Keep CIH graph construction behind an internal adapter so the algorithm dependency does not
  leak into `cih-core`, artifacts, MCP contracts, or wiki APIs.
- Remove the local `leiden.rs` optimizer after parity and rollback validation. Keep a
  `legacy-local-move` implementation behind a temporary CLI fallback for one release.

The selected library must execute all Leiden phases:

```text
fast local movement
  -> refinement within provisional communities
  -> aggregation into a smaller weighted graph
  -> repeat until stable or the configured iteration limit
```

### Input graph

Eligible symbol kinds:

- `Class`
- `Interface`
- `Method`
- `Constructor`
- `Function`

Eligible edges remain `CALLS`, `EXTENDS`, and `IMPLEMENTS`. The graph remains undirected for
community detection while process tracing retains the directed call graph.

Edge construction rules:

- Ignore self-edges.
- Use `max(confidence, 0.01)` as weight.
- Sum weights when multiple eligible edges connect the same pair.
- On large graphs, discard edges below `min_confidence_large` and symbols with filtered degree
  zero or one.
- Sort input node IDs and edge tuples before building the algorithm graph so seeded execution is
  stable across hash-map and parser ordering.

### Configuration and failure behavior

Extend `CommunityConfig` with:

```rust
pub enum CommunityAlgorithm {
    Leiden,
    LegacyLocalMove,
}

pub struct CommunityConfig {
    pub algorithm: CommunityAlgorithm,
    pub quality: CommunityQuality,
    pub resolution: f64,
    pub max_iterations: u32,
    pub seed: u64,
    pub min_confidence_large: f32,
    pub min_community_size: usize,
}

pub enum CommunityQuality {
    Modularity,
}
```

Defaults:

- Algorithm: `Leiden` after acceptance tests pass; `LegacyLocalMove` remains an explicit fallback.
- Quality: modularity.
- Resolution: `1.0` for all repository sizes until the evaluation suite justifies another value.
- Seed: `0xc0de`.
- Large-graph iteration cap: three complete Leiden iterations.
- Minimum emitted size: two normally, three for large graphs, five for monoliths, preserving the
  current discover behavior.

Expose CLI overrides:

```text
cih-engine discover <repo> --community-algorithm leiden|legacy-local-move
cih-engine discover <repo> --resolution <number>
cih-engine discover <repo> --min-community-size <number>
```

Do not convert timeout or algorithm failure into a single giant community. Return a discovery
error, leave the previous complete community artifact untouched, and report the failure in human
and JSON output.

### Output identity and metrics

Keep `Community:<ordinal>` node IDs for one compatibility release. Add a stable community key:

```text
stable_key = first 12 hex characters of BLAKE3(sorted member NodeIds joined with newline))
```

Use `stable_key` for documentation slugs, incremental wiki metadata, and community-detail lookup.
The same graph and configuration must produce the same key. A changed member set intentionally
produces a new key.

Write `.cih/artifacts-community/<version>/metrics.json` containing:

- Algorithm and dependency version.
- Quality function, resolution, seed, and iteration settings.
- Modularity/quality score returned by Leiden.
- Input and clustered symbol counts.
- Community count and size distribution.
- Omitted singleton/small-community counts.
- Connected-community validation result.
- Cohesion distribution.
- Elapsed time.

Add the algorithm, stable key, quality score, symbol count, and cohesion to community node
properties. Preserve existing properties and `MEMBER_OF` edges.

## Agent and MCP Integration

### Community access

Add an MCP tool and resource template:

```text
community_detail({ community, repo?, member_limit?, cursor? })
cih://repo/{name}/community/{stable_key}
```

The response includes:

- Community ID, stable key, label, naming reason, cohesion, and symbol count.
- Representative symbols ordered by internal weighted degree.
- Paginated members ordered by kind, file, and node ID.
- Routes, controllers, tables, topics, tests, and participating processes.
- Incoming and outgoing community connections with edge counts and total weights.
- Quality warnings such as low cohesion, oversized community, or generic label.

Default `member_limit` is 50 and maximum is 200. Cursor pagination must be deterministic and
based on the sorted member position.

### Retrieval workflow

Expose `get_community` to `AgentRunner` alongside `search_code`, `get_context`, and
`trace_impact`. Update the system prompt to use this sequence for broad questions:

```text
search_code
  -> group seed hits by community
  -> rank matching communities
  -> expand at most three communities
  -> inspect representative symbols and relevant processes
  -> verify claims with symbol context or call edges
```

Rank communities using reciprocal-rank fusion over their matched search hits. Break ties by
higher cohesion, then stable key. Do not boost an unmatched community solely because it is large.

Exact NodeId lookup, direct method tracing, and explicit impact questions continue to use symbol
context and graph traversal without mandatory community expansion.

Agent responses may describe a community as a navigation grouping, but every behavioral claim
must cite source symbols or edges. Low-cohesion or generic-label communities must be identified as
uncertain rather than presented as business modules.

## Separate Documentation Tree

### Generated layout

Generate two sibling branches below `.cih/wiki/pages/`:

```text
pages/
  index.md

  codebase/
    _category_.json
    index.md
    routes.md
    features/
      <feature>/
        index.md
        po.md
        ba.md
        dev/
          <class>.md
          <class>.json

  communities/
    _category_.json
    index.md
    graph.md
    graph.json
    communities.json
    <community-label>-<stable-key>/
      index.md
      community.json
```

Category order:

1. Codebase
2. Communities

The root index links to both branches and explains that codebase documentation is organized for
product, analysis, and development use, while community documentation exposes detected graph
structure and quality.

### Codebase branch

- Move the existing system overview, routes, feature PO/BA pages, controller pages, and class
  technical references under `codebase/`.
- Continue using communities internally to infer feature ownership, but do not mix raw community
  pages into the codebase feature hierarchy.
- Add explicit frontmatter slugs to preserve current public routes for existing feature and route
  pages even though their generated files move under `codebase/`.
- Restrict stale-file cleanup to the generated `codebase/` branch.

### Communities branch

`communities/index.md` contains:

- Algorithm/configuration summary.
- Quality score and connected-community status.
- Size and cohesion distributions.
- Warnings for fragmentation, giant communities, generic labels, and omitted small communities.
- A table of documented communities linked by stable slug.

`communities/graph.md` renders the inter-community graph. Its JSON sidecar contains weighted
community nodes and links for the viewer.

Each community `index.md` contains:

- Name, stable key, symbol count, cohesion, and quality warnings.
- Business signals: routes, tables, topics, controllers, and processes.
- Top representative symbols.
- Incoming and outgoing community dependencies.
- Member table capped at 100 rows in Markdown.
- Links from members to their codebase class pages when such pages exist.

`community.json` contains the complete sorted member list and graph slice for programmatic use.

Documentation generation defaults to communities with at least five symbols. Add
`--community-docs-min-size`, defaulting to five. Smaller communities remain in
`communities.json` and aggregate metrics but do not receive individual Markdown pages.

### Manifest compatibility

Bump the wiki manifest schema to version 2 and add:

- `PageEntry.branch`: `codebase` or `communities`.
- `PageEntry.stable_community_key` for community pages.
- Community algorithm and quality metrics.
- Documentation minimum community size and omitted-community count.

New fields use serde defaults so schema-1 manifests remain readable. Existing page roles and
community IDs remain available during the compatibility release.

The Docusaurus viewer continues using the autogenerated sidebar. `_category_.json` files create
the two branches; no manually maintained sidebar entries are introduced.

## Testing and Evaluation

### Algorithm tests

- Two dense cliques joined by a weak bridge produce two communities.
- Every emitted community passes an internal BFS connectivity check.
- Repeated runs with identical input and seed produce identical memberships and stable keys.
- Parallel eligible edges accumulate confidence weight correctly.
- Low-confidence and degree-one filtering remains deterministic.
- `Function` nodes participate in TypeScript and Python fixtures.
- Empty, edgeless, disconnected, and cyclic graphs return valid results without a giant fallback.
- Algorithm errors preserve the last complete artifact version.
- Legacy mode reproduces the current local-move assignments for locked fixtures.

### Repository evaluation

Run both algorithms on small synthetic fixtures, Spring Petclinic, Fineract, and representative
TypeScript/Python repositories. Record:

- Community count, median and percentile sizes, and largest-community share.
- Modularity/quality and cohesion distributions.
- Disconnected-community count, which must be zero for Leiden.
- Tiny-community and unclustered-symbol fractions.
- Runtime and peak memory.
- Stability across two unchanged runs, which must be exact.

Do not claim that GitNexus-quality clustering has been reached solely because Leiden is present.
Graph resolution quality and edge selection remain separate measured inputs.

### Agent A/B evaluation

Use the same golden question set with community expansion disabled and enabled:

- Exact symbol questions must have no retrieval or grounded-answer regression.
- Broad feature/architecture questions must improve Recall@10 by at least 10 percentage points,
  or reduce tool calls/tokens by at least 20% with no grounded-claim regression.
- Unsupported questions must retain abstention behavior.
- Record tools called, retrieved node IDs, community expansions, citations, tokens, and latency.

Community expansion becomes the default for broad questions only after these criteria pass.

### Documentation tests

- Snapshot the generated two-branch tree.
- Verify category ordering, frontmatter slugs, manifest schema 2, stable community links, and JSON
  sidecars.
- Verify small communities are summarized but do not receive individual pages by default.
- Verify every community member link resolves to either a codebase page or a source file reference.
- Generate Fineract docs and confirm the community branch does not create pages for the existing
  1,106 communities of size three to five.
- Run the Docusaurus production build and fail on broken internal links.

Final verification:

```bash
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets
cd docs-viewer && npm run build
```

## Rollout

1. Add `leiden-rs`, the adapter, metrics, connectivity validation, and locked algorithm tests.
2. Run legacy and Leiden discovery side by side on the evaluation repositories.
3. Add stable keys and community-detail MCP access without changing the default agent workflow.
4. Generate the separate `codebase/` and `communities/` documentation branches and validate URL
   compatibility.
5. Enable community expansion for broad agent questions after the A/B criteria pass.
6. Make Leiden the default and retain `--community-algorithm legacy-local-move` for one release.
7. Remove the legacy implementation after one release with no blocking regression reports.

## Assumptions and Boundaries

- Communities are non-overlapping; one symbol belongs to one final partition community.
- CIH persists only the final partition in V1 even though Leiden aggregates internally.
- Community labels remain heuristic/LLM-enrichable metadata and are never treated as ground truth.
- Confidence-weighted undirected clustering remains the default; directed process tracing is a
  separate stage.
- Community docs are a sibling of codebase docs, not nested inside feature pages.
- Community docs expose graph structure; codebase docs remain the primary PO/BA/developer
  documentation experience.
- Incremental Leiden warm starts and overlapping communities are deferred until the full rebuild
  implementation is validated.
