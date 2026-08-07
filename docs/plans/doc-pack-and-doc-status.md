# doc_pack + doc_status — per-node documentation evidence MCP tools

Status: Proposed, revised after design review (2026-08-01). Tool count goes 33 → 35.

## Context

Operators in egress-restricted environments cannot let the server (or the engine) call
external LLM APIs, yet still need per-endpoint / per-symbol documentation. CIH's
architecture already keeps natural language in the MCP client (the server is
LLM-egress-free, `SECURITY.md`), so the resolution is a server tool that returns a
**curated, bounded documentation evidence pack for one node**: the MCP client's agent
(Kiro, Claude, …) writes the prose through its own approved channel.

One `doc_pack` call replaces the 5–10-call ad-hoc chain (`trace_flow` + `context` +
`read_file` + …), constrains hallucination (the agent writes only from curated
evidence with explicit completeness), and includes a deterministic markdown skeleton
so LLM-less clients still get a usable page. A `doc_status` companion makes
regeneration genuinely incremental: generated pages carry a hash of their bounded,
node-local evidence plus the exact effective pack profile; after re-analyze,
`doc_status` rebuilds that same profile and lists only pages whose evidence changed.

Scope decisions (agreed 2026-07-29): JSON + markdown skeleton in one response; node
kinds Route/Method/Function/Constructor/Class/Interface; staleness workflow (hash +
status tool) included in v1.

## Design invariants

1. **Per-node staleness.** A repository-wide graph version, publication epoch,
   indexing timestamp, or contract-sync timestamp is provenance, not hash input. An
   unrelated repository change must not stale every page.
2. **Reproducible profile.** The exact effective profile used for the returned pack is
   serialized in both JSON and frontmatter. `doc_status` never guesses defaults for an
   existing page.
3. **One coherent repository context.** Resolve the repo once per build, use the same
   version-bound context for every graph/file section, and detect a publication change
   during the build.
4. **Bound work before rendering.** Every collection has a query/scan cap and explicit
   completeness. The response byte backstop is a final transport safeguard, not the
   primary bound.
5. **Hash what was delivered.** If the byte backstop drops a section, remove it from
   the effective profile, recompute the fingerprint/hash, and regenerate markdown.
6. **Honest degradation.** A requested section is either bounded evidence or an
   explicit unavailable section. A current runtime/store failure prevents
   `doc_status` from claiming freshness.

## Tool 1: `doc_pack`

### Arguments and effective profile

`DocPackArgs` has one required argument and four optional arguments:

- `name`: symbol name or full NodeId.
- `repo`: registry name/path; empty means the primary repo.
- `group`: optional group enabling route-scoped contract consumers.
- `include_source`: default `true`; follow the existing MCP args convention with
  `#[serde(default = "default_true")]` on a plain `bool` (`args.rs:461-476`).
- `sections`: optional subset of `flow`, `upstream`, `tests`, `source`, `contracts`;
  absence means all five. Model it as `Option<Vec<String>>` so an explicitly supplied
  empty array can be rejected instead of surprisingly meaning “everything”. Identity
  is mandatory and is not selectable.

`DocPackCommand::try_new` trims inputs, validates/deduplicates sections, and normalizes
them into declaration order. `sections: []` is invalid. `include_source=false` removes
`source` from the effective section set; reject the command if that leaves an explicit
selection empty (for example `sections:["source"], include_source:false`).

The versioned profile is:

```rust
struct EvidenceProfileV1 {
    schema: u8,                 // always 1
    group: Option<String>,
    include_source: bool,
    sections: Vec<DocSection>, // normalized effective/delivered sections
}
```

The effective profile is hash input. `DocPackOutput` also carries a
`requested_profile`, normalized before response-size drops. The returned effective
profile may contain fewer sections only when the byte backstop drops sections; every
such drop is named in `warnings`. The requested profile is retained so a later
regeneration can retry the caller's intended evidence instead of making a backstop
drop permanent. Command validation rejects an explicit empty selection, but the
frontmatter profile parser accepts an empty **effective** section list because the
backstop may legitimately reduce a pack to mandatory identity only; its accompanying
requested profile must still be non-empty. `requested_profile` is regeneration metadata
and is not hash input.

### Behavior by kind

- **Route** — flow starts at the route node. Contracts are available only when the
  section is requested, `group` is present, and the node has `httpMethod` + `path`.
  Contract matches are scoped to the resolved provider repo **and exact route NodeId**,
  not just method/path.
- **Method/Function/Constructor** — flow starts at the callable; contracts are
  `available:false` with stable unavailable code `routes_only`.
- **Class/Interface** — flow is `available:false` with code `member_required` and a
  remedy naming `trace_flow` on a member; upstream/tests/source still apply. The new
  paged test query must include both direct tests of the class/interface and tests
  targeting its indexed members via outgoing `HAS_METHOD`. This is intentionally
  broader than existing `test_coverage`, whose `HAS_METHOD` branch only rolls a queried
  member up to tests of its owner. Contracts are `available:false` with `routes_only`.
- Any other kind returns `AppError::InvalidInput` naming the supported kinds.

### Response shape and section bounds

`DocPackOutput` uses `Option<Section<T>>` for selectable sections: `None` means not
requested or dropped by the byte backstop; `available:false` means requested but not
available.

```text
{ repo, node_id, requested_profile, profile, evidence_hash, graph_version?,
  identity:  { node fields + curated props (httpMethod, path, stereotype,
               cyclomatic, cognitive, transitiveLoopDepth, isRecursive) },
  flow?:     { steps, db_effects, completeness },
  upstream?: { callers, processes, completeness },
  tests?:    { scope, test_count, tests, completeness },
  source?:   { path, start_line, end_line, truncated, content },
  contracts?:{ consumers, completeness, contracts_synced_at, contracts_stale },
  markdown:  "<rendered skeleton>",
  warnings:  [ ... ] }
```

Hard bounds, applied before fingerprinting/rendering:

- flow: `FLOW_MAX_DEPTH = 6`, `FLOW_MAX_NODES = 100`, `business_only=true`;
- upstream: at most 50 callers and 25 processes;
- tests: at most 50 test nodes, fetched with `limit + 1` and returned with
  completeness (`test_count` is the number returned, not an asserted total when the
  section is incomplete);
- source: at most 120 lines and 8 KiB, truncated on a UTF-8 character boundary;
- contracts: at most 50 consumers, found with a streaming `limit + 1` scan; cap the
  scan independently at `CONTRACT_SCAN_MAX_ROWS = 50_000` parsed rows and
  `CONTRACT_SCAN_MAX_BYTES = 8 MiB` read bytes so a malformed or enormous artifact
  cannot make the request unbounded. Hitting either scan cap before EOF makes the
  section incomplete even if fewer than 51 matching consumers were found.

The tests body exposes a stable scope label. Class/Interface pages use
`direct_and_members`; Method/Constructor pages preserve existing owner roll-up
semantics with `direct_and_owner`; Function/Route pages use `direct`. An empty complete
class or interface result renders “No tests target this type or its indexed members.”
An empty complete method or constructor result renders “No tests target this callable
or its owning type.” The generic “No tests target this symbol” wording is used only for
a complete `direct` scope. Any incomplete empty result is described as inconclusive.

### Shared sections and error model

Promote `Section<T>` and `Section::ok/off/store_err` from
`application/architecture_overview.rs:256-298` to
`application/section.rs`. The type and constructors are `pub(crate)` so sibling
application modules can use them; architecture-overview serialization remains
unchanged. Its `remedy` module stays local.

The serialized `Section<T>` shape remains `available/source/body` or
`available/reason/remedy`. Hashing uses a separate internal representation:

```rust
enum FingerprintSection<T> {
    Available(T),
    Unavailable { code: UnavailableCode },
}
```

Human-readable reasons, remedies, warnings, and backend error text are never hash
input. Deliberate unavailability (`routes_only`, `member_required`, `group_required`,
`missing_route_props`) has a stable code. A runtime/store error uses stable fingerprint
code `runtime_error` and is also rendered as an unavailable section by `doc_pack`, but
the builder records `had_runtime_error=true`; `doc_status` returns `status:"error"`
rather than comparing a partial current hash.

### Evidence fingerprint and hash

`EvidenceFingerprintV1` contains exactly:

```text
schema tag + node_id + EvidenceProfileV1 + identity +
the profile-selected FingerprintSection bodies
```

It excludes `graph_version`, publication epoch, indexed time,
`contracts_synced_at`, `contracts_stale`, warnings, remedies, markdown, and all other
call-time/provenance fields. Contract *consumers* and completeness remain hash input.
All vectors are explicitly sorted by stable keys before serialization; do not rely on
backend traversal order alone. Render JSON/markdown from those same sorted values so
regeneration is byte-deterministic. Identity deliberately includes the displayed
complexity properties, so a complexity change to the documented node stales its page.

Do not hash a raw `serde_json::Value` props map. Project identity into concrete typed
fields shared by response/rendering/fingerprinting: strings for `httpMethod`, `path`,
and `stereotype`; canonical non-negative `u64` values for `cyclomatic`, `cognitive`,
and `transitiveLoopDepth`; and `bool` for `isRecursive`. Typed extractors normalize
equivalent integral JSON number representations (and documented numeric-string legacy
forms) to the same `u64`; malformed/out-of-range values become absent with a warning.
This prevents backend number formatting from causing false staleness.

`evidence_hash(&EvidenceFingerprintV1) -> String` is the first 32 lowercase hex
characters of blake3 over `serde_json::to_vec` of that concrete struct. Both tools call
the same builder and this single hash function.

`graph_version: Option<String>` remains in JSON/frontmatter for diagnostics and comes
from the resolved registry entry's optional `published_graph_content_version`; it is
not staleness input. When it is `None`, omit `cih_graph_version` from frontmatter
rather than writing an empty scalar.

### Byte self-cap

Use the architecture-overview pattern with
`DOC_PACK_MAX_RESPONSE_BYTES = 64 KiB`, a 512-byte envelope margin, and
`DROP_ORDER = [source, contracts, flow, upstream, tests]`. Identity is never dropped.
This deliberately doubles architecture-overview's 32 KiB self-cap because a doc pack
contains both a source excerpt and a Mermaid-backed markdown rendering; it remains far
below the transport's 256 KiB soft warning target.

On each drop:

1. remove the section from `DocPackOutput` and effective `profile.sections` while
   leaving `requested_profile` unchanged;
2. append a warning naming the section and standalone re-fetch tool;
3. rebuild `EvidenceFingerprintV1` and `evidence_hash`;
4. regenerate markdown/frontmatter from the new effective profile;
5. serialize and measure again, including warnings.

If the irreducible identity/metadata response still cannot fit, return a typed
`AppError::Unavailable` instead of returning an oversized response.

## Markdown skeleton

`application/doc_pack/render.rs` contains pure
`render_doc_page(&DocPackOutput) -> String` and section renderers. Frontmatter is:

```yaml
---
title: "<JSON/YAML-safe METHOD path or qualified name>"
cih_node: "<JSON/YAML-safe NodeId>"
cih_evidence_hash: <32 lowercase hex>
cih_graph_version: "<provenance version>"
cih_generator: doc_pack-v1
cih_profile: {"schema":1,"group":null,"include_source":true,"sections":["flow","upstream","tests","source","contracts"]}
cih_requested_profile: {"schema":1,"group":null,"include_source":true,"sections":["flow","upstream","tests","source","contracts"]}
---
```

Both profiles are compact canonical JSON on one physical line; strings are emitted
with `serde_json::to_string`. `doc_status` parses `cih_profile` for comparison and
surfaces whether it differs from `cih_requested_profile`. This avoids inventing a
partial YAML parser for profile values while preserving the caller's pre-backstop
intent. The example shows a published repo; the renderer omits the
`cih_graph_version` line entirely when `graph_version` is `None`.

Body layout:

```text
# <title>
<!-- cih:prose:overview:start -->
<!-- cih:prose:overview:end -->
## Facts
## Execution flow
<!-- cih:prose:flow:start -->
<!-- cih:prose:flow:end -->
## Data access
## Callers & processes
## Tests
## Source
## Cross-repo consumers
<!-- cih:prose:notes:start -->
<!-- cih:prose:notes:end -->
```

Requested-but-unavailable sections render a one-line italic note with their remedy.
Unrequested/backstop-dropped sections are absent; a backstop drop is still explicit in
the JSON `warnings` returned to the caller. Source fences choose a delimiter longer
than any backtick run in the excerpt.

Flow steps remain sanitized `cih_graph_store::FlowHop` values. Each hop retains its
nested `hop.node: FlowNode`, including `node.parent_id`, `node.intercepted_by`,
`node.qualified_name`, `node.file`, and `node.depth`. `hop.via` is
`Option<FlowEdge>` (`None` for the root); when it is `Some`, retain `via.kind` but clear
`via.call_sites`. This controls argument-text size while allowing direct reuse of
`viz::render_mermaid_flow` (`cih-server/src/viz.rs:13`). Execution flow and Data access
are one flow-owned rendering unit: render Data access only when flow evidence is
available, so a class page does not show an empty Data access section beneath an
unavailable flow.

### Prose-preserving regeneration contract

`doc_pack` always returns a fresh deterministic skeleton; it does not read or merge an
existing documentation page. Before replacing a stale page, the MCP client/agent must:

1. extract each existing agent-authored prose block between its matching
   `<!-- cih:prose:<name>:start -->` and `<!-- cih:prose:<name>:end -->` sentinels;
2. translate the page's `cih_requested_profile` fields back into the real `doc_pack`
   arguments (`group`, `include_source`, and `sections`), rather than using a reduced
   effective profile left by a prior byte-backstop drop;
3. splice the preserved prose blocks into the matching markers in the fresh skeleton;
4. review/update prose whose claims conflict with changed evidence, then replace the
   page atomically while keeping the new frontmatter.

If markers are missing, duplicated, or structurally ambiguous, the workflow must not
overwrite the page automatically; it reports that manual reconciliation is required.
`docs/agent-workflows/documenting.md` specifies this algorithm and includes a concrete
before/after example.

## Tool 2: `doc_status`

### Arguments

`repo`, `docs_dir` (default `"docs"`), `max_pages` (default 100, clamp 1..=500).

### Safe deterministic scan

1. Resolve the repo once and canonicalize `<repo>/<docs_dir>` with the same
   root-containment rules as `FileService`. Reuse the shared contained-path helper that
   implementation step 4 extracts from the two existing inline checks; do not create a
   third containment implementation in `doc_status`.
2. Walk to depth 4 without following symlinks. Sort directory entries by file name,
   cap visited entries separately (for example 10,000), and return incomplete when
   that traversal cap is hit.
3. For each `*.md` beginning with `---`, read through the closing `---` delimiter,
   subject to a 16 KiB total header cap. Use a byte-limited reader so even one enormous
   line is bounded. If the opening delimiter has no closing delimiter within 16 KiB,
   return an `unparseable` row (`frontmatter_too_large`) rather than silently ignoring
   a possible CIH page. Do not impose a 20-line limit: ordinary author-added keys may
   legitimately place CIH keys later in the frontmatter.
4. Ignore ordinary Markdown with no frontmatter or a complete frontmatter block with
   no `cih_` keys; ignored files do not consume `max_pages`. If any CIH key is present,
   require valid `cih_node`, `cih_evidence_hash`, `cih_generator: doc_pack-v1`,
   `cih_profile`, and `cih_requested_profile`; partial or malformed CIH metadata becomes
   `unparseable`. `cih_graph_version` is optional diagnostics-only provenance, so its
   absence does not make a page unparseable.
5. Stop after `max_pages + 1` CIH candidate pages. Sort returned rows by relative path;
   the over-fetched row is used only to set `completeness.complete=false`.

### Comparison

Deduplicate by `(node_id, EvidenceProfileV1)`. For each unique pair, call the same
evidence builder with rendering disabled and exactly the frontmatter profile. Compare
the resulting hash with the stored hash. Do not use stored `cih_graph_version` in the
comparison.

Output:

```text
DocStatusOutput {
  pages: [{ path, node_id?, status, stored_hash?, current_hash?,
            profile_reduced?, reason? }],
  completeness
}
```

`status ∈ fresh | stale | missing_node | unparseable | error`. A missing symbol maps
to `missing_node`; any current runtime/store error needed for a requested section maps
to `error` for every page sharing that `(node, profile)`. One page's error never fails
the whole call. Run rebuilds with small bounded concurrency (for example four tasks),
not 500 simultaneous graph/file operations.

`profile_reduced=true` means the prior `doc_pack` byte backstop dropped at least one
requested section. Such a page can correctly remain `fresh` forever under its reduced
effective profile; this is intentional rather than a promise that the omitted section
will reappear. The workflow tells agents to call `doc_pack` with
arguments reconstructed from `cih_requested_profile` when they want to retry the
intended full pack, even when the current reduced page is fresh.

Orphan detection (nodes without pages) remains out of scope for v1; `route_map` already
covers “which endpoints exist” for agents.

## Service composition and snapshot consistency

`DocPackService` owns clones of:

```text
RepoContextService, GraphQueryService, TestingService, FileService, ContractService
```

Add this new crate-local value type (it is doc-pack infrastructure, not an existing
repository mechanism):

```rust
struct RepoSnapshotToken {
    published_epoch: Option<String>,
    published_graph_content_version: Option<String>,
    indexed_at: String,
}
```

Both published fields are legitimately `None` for repositories loaded through paths
that have never recorded a publication. Token equality compares the full tuple; in
that mode `(None, None, indexed_at)` is the actual before/after consistency check—not
an attempt to unwrap a missing publication field. At the start of a pack/status build,
resolve one `Arc<RepoContext>` and construct the token. Factor internal
`*_in_context` entry points from graph/testing/file services so they accept that
resolved context and do not independently resolve the repo again. Existing MCP-facing
methods continue to resolve and then delegate to those helpers.

After evidence construction, re-read the registry token. `doc_pack` retries the whole
build once when the token changed, then returns `Unavailable` if publication changes
again. `doc_status` aborts with a tool-level retryable `Unavailable` if its repository
snapshot changes during the batch; it must not emit a mixture of versions.

Initial symbol resolution calls `resolve_symbol(&context.store, name)`, which returns
`SymbolResolution::{Id, Ambiguous, NotFound}` rather than a transport/output wrapper.
`DocPackService::execute` returns
`Result<SymbolQueryOutput<DocPackOutput>, AppError>`: it maps `Ambiguous(nodes)` to
`SymbolQueryOutput::Ambiguous(AmbiguousResult::from_nodes(nodes))`, maps `NotFound` to
the standard symbol error, and calls `get_node` after `Id` for the mandatory
identity/kind gate.

One original constraint is intentionally relaxed: add a paged graph-store operation
for honest test completeness. Existing Falkor and Ladybug `test_coverage` queries are
already bounded by a hard-coded `LIMIT 50`, but they silently truncate. Add
`test_coverage_page(id, limit)` returning `{tests, has_more}` to `GraphStore`, Falkor,
Ladybug, and test fakes, with caller-selected bounds, deterministic ordering (including
an ID tie-breaker), and backend `LIMIT limit + 1` over-fetch. Keep existing
`test_coverage` query semantics and public-tool behavior unchanged; it must not delegate
to the kind-aware method because Class/Interface results intentionally differ. No other
new store ports/queries are required.

The new method's contract is kind-aware: always include direct `TESTS` targets; retain
the existing member→owner roll-up for queried Method/Constructor ids; and, when the
queried node is Class or Interface, also include tests whose target is a node reached
by the queried type's outgoing `HAS_METHOD`. Resolve the queried kind inside the
backend query (or pass an equivalent validated scope internally), deduplicate tests,
then order and over-fetch. A contract-suite fixture with **no direct class test** and
only a test targeting one member must return that test for the class query.

For contracts, add an internal bounded `ContractService::api_impact_for_route` taking
`group`, `provider_repo`, `provider_route`, `method`, `path`, and `limit`. It streams
contract rows, filters by all provider fields plus normalized match key, does not load
consumer caller graphs, and returns a `pub(crate)` projection with completeness and
freshness provenance. This method uses a **new** byte-capped `BufReader` line reader;
it does not call existing `load_group_contracts`, which uses unbounded
`read_to_string`, and it does not reuse `api_impact_sync`, whose filter ignores provider
identity. It enforces both the byte and parsed-row caps above and reports incomplete
when either stops the scan before EOF. Compare the current route's
`node.id.as_str()` with the stored
`ContractMatch.provider_id` only after a fixture/integration test proves that
group-sync's `node.id.as_str().to_string()` write emits the expected canonical
`Route:METHOD PATH` format. A mismatch is a contract format error, not “no consumers.”
The existing public `api_impact` tool and loader are unchanged.

## Implementation steps (build order)

1. **Shared section type:** add `application/section.rs`; move `Section<T>` and its
   three constructors as `pub(crate)`; update architecture-overview imports and prove
   its serialized shape is unchanged.
2. **Paged tests port:** add `TestCoveragePage` + `test_coverage_page` to
   `cih-graph-store` alongside the existing fixed-limit `test_coverage` operation;
   implement the new method with deterministic, caller-bounded `limit + 1` queries in
   Falkor and Ladybug. Preserve the old method's legacy backend query rather than
   delegating it to the new kind-aware method; in particular, public Class/Interface
   results must not gain member-targeting tests. Explicitly add paged coverage to the
   hand-written `run_contract_suite` and update every server fake—the contract suite
   does not discover new trait behavior automatically.
3. **Route-scoped contracts:** implement the new byte-capped `BufReader` JSONL loader
   and bounded internal contract method alongside (not as a wrapper over)
   `load_group_contracts`/`api_impact_sync`. Add pure filtering tests for two provider
   repos exposing the same method/path plus a group-sync fixture proving
   `provider_id == selected_node.id.as_str()`.
4. **Contained-path helper:** extract one shared canonical root/target containment
   helper from the existing inline read-file and literal-glob symlink checks in
   `application/files/mod.rs`; retain their field-specific error wording and use the
   helper from `read_file`, `plan_walk`, and `doc_status` rather than adding a third
   copy.
5. **Context-bound helpers:** factor graph trace/context, bounded test coverage, and
   file read helpers that operate on an already-resolved `RepoContext` without
   changing existing public tool behavior.
6. **`application/doc_pack/mod.rs`:** implement commands, profile, bounded section
   projections, snapshot guard, fingerprint/hash, response backstop, and `doc_status`.
   Give status rebuilding an internal entry point that accepts an already-normalized
   `EvidenceProfileV1`, including an empty effective `sections` list from frontmatter;
   it must not route back through `DocPackCommand::try_new`, whose caller-input contract
   correctly rejects an explicitly empty selection.
7. **`application/doc_pack/render.rs`:** implement safe frontmatter, structural section
   renderers, source fences, and the new sanitizer that clones full `FlowHop`s and
   clears `via.call_sites` when present before Mermaid rendering.
8. **Application wiring:** add `doc_pack: DocPackService` to `DocsUseCases`; in
   `bootstrap::assemble_services`, construct named graph/testing/file/contract service
   values, clone them into `DocPackService`, and retain the existing use-case owners.
9. **Transport args/tools:** add `DocPackArgs` (`sections: Option<Vec<String>>`,
   `include_source` using `default_true`), `DocStatusArgs`, the new
   `tools/doc_pack.rs` router, `doc_pack_service()` accessor, module registration, and
   `+ CihServer::doc_pack_router()`. The distinct `tools/doc_pack.rs` name avoids the
   existing wiki `tools/docs.rs` module. Tool descriptions use only real schema arg
   names.
10. **Tool surface tests:** update tool count 33 → 35 in
   `transport/mcp/server.rs` and `transport/mcp/dispatch_tests.rs`; leave `HINT_TOOLS`
   unchanged for v1.
11. **Documentation:** update `CLAUDE.md`, `README.md`, `USAGE.md`, and the agent
    workflow matrix; create the new `docs/agent-workflows/documenting.md` describing
    `route_map → doc_pack → preserve frontmatter → doc_status → regenerate stale`,
    including the “claim only what's in the delivered pack” rule, marker-based prose
    extraction/splicing, manual-conflict behavior, and retrying `cih_requested_profile`
    when a prior response used a reduced profile. Document the known source-freshness
    limit: the hash covers only the delivered 120-line/8-KiB excerpt, so an edit beyond
    its truncation point may not stale the page unless it also changes the symbol's line
    span, typed identity metrics, or another delivered evidence field. Also explain that
    Route test scope is intentionally direct-only: an empty Route test section does not
    establish that its handler is untested, and agents should call `test_coverage` on
    the first callable in the delivered flow when they need handler-level coverage.

## Tests

- **Transport args:** absent sections → all, explicit `sections:[]` rejected,
  `include_source` absent → true / explicit false preserved, invalid sections,
  deduplication/order normalization, max-page clamp, and malformed profiles.
- **Section move:** architecture-overview serialization before/after remains equal;
  sibling modules can call the `pub(crate)` constructors.
- **Pack behavior:**
  1. Route pack returns bounded sections, DB effects, profile, hash, safe frontmatter,
     and prose markers.
  2. Duplicate method/path contracts in two providers return consumers only for the
     current `(provider_repo, route NodeId)`; canonical group-sync `provider_id`
     matches `node.id.as_str()`, while a deliberately mismatched format reports a
     contract-format error rather than an empty consumer list.
  3. Short ambiguous name preserves the standard serialized ambiguous result.
  4. Class/interface flow is unavailable with `member_required`; tests include direct
     type tests plus member-targeting tests and report scope `direct_and_members`. A
     class with no direct test but one tested method must not render a no-tests claim.
  5. One section failure degrades only that section, records a warning/runtime-error
     flag, and does not corrupt identity.
  6. Tests/contracts use `limit + 1`, expose incomplete bounds, and never serialize
     the over-fetched item.
  7. Sanitized flow clears call-site args only when `hop.via` is `Some`, preserves a
     root `via:None`, retains `hop.node` parent/interception data, and renders Mermaid
     edges.
  8. Unsupported kinds fail before section work.
- **Hash contract:**
  1. Identical node-local evidence + profile yields identical hashes.
  2. Changing a DB effect, caller, test, source excerpt, or scoped consumer changes the
     hash.
  3. Changing graph version, publication epoch, indexed time, contract-sync timestamp,
     warning text, remedy text, or markdown does **not** change the hash.
  4. Changing group/include-source/effective-sections changes the profile/hash.
  5. Every collection is sorted once and the same order drives hashing, JSON, and
     markdown rendering.
  6. Equivalent complexity props represented as integral JSON number variants or
     documented numeric strings project to identical typed `u64` fingerprint fields;
     malformed/negative values are omitted with a warning rather than hashed raw.
- **Byte cap:** force each drop path; assert the section is removed from output and
  effective profile while remaining in `requested_profile`; hash/frontmatter are
  recomputed, markdown omits it, warnings name it, and final serialized bytes plus
  margin fit 64 KiB. A reduced-profile page remains fresh under that profile and
  advertises `profile_reduced=true`.
- **`doc_status`:**
  1. default, non-default, and backstop-produced empty effective profiles compare
     fresh; the empty-profile rebuild bypasses caller-command validation;
  2. node-local mutation becomes stale while an unrelated graph-version change stays
     fresh;
  3. absent node → `missing_node`; current store failure → `error`;
  4. ordinary Markdown is ignored; CIH keys after line 20 but before the closing
     frontmatter delimiter are found; unclosed/over-16-KiB or partial CIH frontmatter
     is `unparseable` rather than ignored;
  5. deterministic path order, candidate-only `max_pages + 1`, visited-entry cap,
     header-byte cap, and symlink skipping;
  6. duplicate `(node, profile)` pages trigger one evidence rebuild;
  7. publication-token change prevents mixed-version results.
- **Renderer:** section order, escaped frontmatter, adaptive source fence, Mermaid
  fence when flow is available, Data access omitted when flow is unavailable, honest
  kind/scope-aware incomplete-tests wording, unavailable note, and omission of
  `cih_graph_version` when the registry value is `None`.
- **Snapshot token:** equality covers `Some` publication values and the unpublished
  `(None, None, indexed_at)` mode; changing only `indexed_at` in unpublished mode trips
  the consistency guard.
- **Contained paths:** shared-helper tests cover contained files, outside-root
  symlinks, exact contained file symlinks, directory-link rejection, and field-specific
  error labels for read/glob/status callers.
- **Workflow fixture:** extract all three prose blocks from an existing page, render a
  changed skeleton, splice the blocks back without altering them, and refuse automatic
  replacement when markers are missing or duplicated.
- **Dispatch:** count 35; empty `name` fails validation before repo/symbol resolution;
  descriptions and schemas remain consistent.

## Verification

1. Run `cargo fmt --all --check`,
   `cargo clippy --workspace --all-targets -- -D warnings`, and
   `cargo test --workspace` (record rather than mask any known pre-existing failure).
2. Copy `crates/cih-engine/tests/corpus/js-cjs-express` to a temporary repository,
   analyze/load it, start `cih-server`, and call `doc_pack` for a real route. Write the
   returned markdown under that temporary repo's docs directory; `doc_status` must say
   `fresh`.
3. Make a real content change to the selected route/handler, re-analyze, and verify its
   page becomes `stale`. Restore it, then change an unrelated source file, re-analyze,
   and verify the selected page remains `fresh` even though `graph_version` changed.
4. Exercise a non-default profile (`include_source=false`, explicit sections, group)
   and verify `doc_status` reconstructs it as `fresh` without caller-supplied defaults.
   Force a backstop drop, confirm `profile_reduced=true`, then regenerate with
   `cih_requested_profile` and verify preserved prose blocks survive the replacement.
5. In a graph fixture where a class has no direct `TESTS` edge but one member does,
   verify the class pack includes that test, reports `direct_and_members`, and does not
   render a no-tests statement.
6. In a group fixture where two providers expose the same method/path, verify the pack
   includes only consumers of the selected provider route.
7. Confirm serialized `DocPackOutput` stays within its 64 KiB self-cap and the MCP
   envelope remains below the default 256 KiB soft warning target
   (`DEFAULT_MCP_RESPONSE_TARGET_BYTES`); the separate default hard rejection limit is
   1 MiB (`DEFAULT_MCP_RESPONSE_MAX_BYTES`).
8. Stop the temporary server and remove only the temporary corpus/repository artifacts.

## Risks / mitigations

1. **Shared `Section<T>` churn:** keep the serialized type unchanged, expose only
   crate-local constructors, and retain architecture-overview serialization tests.
2. **Hash/profile drift:** one concrete fingerprint struct, one canonical profile
   normalizer/parser, one builder, and one hash function serve both tools.
3. **Mixed publication snapshots:** context-bound helpers plus before/after publication
   token checks over the exact optional-field tuple; retry one pack, fail a changing
   status batch.
4. **Paged-store expansion:** `test_coverage_page` is the only new graph-store method;
   explicit `run_contract_suite` cases cover Falkor/Ladybug ordering, caller-selected
   limits, class/interface member coverage, and over-fetch semantics that the existing
   silent `LIMIT 50` cannot expose.
5. **Response size:** query/scan caps first, then measured whole-section drops, then the
   existing transport guard as backstop.
6. **Filesystem scan abuse:** root containment, no symlink following, deterministic
   depth/entry/page/header-byte caps, and bounded rebuild concurrency.
7. **Prose loss on regeneration:** the server remains read-only with respect to docs;
   the documented client workflow extracts marker-owned prose, uses the requested
   profile, refuses ambiguous pages, and atomically splices prose into the new skeleton.
8. **Contract format/scan drift:** the doc-specific reader is new bounded code, with
   byte/row limits, malformed-line handling, canonical provider-ID fixtures, and no
   behavioral change to the existing unbounded public-tool loader.

## Out of scope (explicit)

Orphan detection in `doc_status`; LLM calls of any kind server-side; batch multi-node
packs (agents loop `route_map → doc_pack`); wiki-pipeline integration (the wiki remains
the exhaustive skeleton generator); the `is_embeddable_kind` Function gap; modifying
the existing public `api_impact` or `test_coverage` response contracts.
