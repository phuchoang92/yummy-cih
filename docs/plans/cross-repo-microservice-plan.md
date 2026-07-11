# Resolve CIH multi-repo / microservice limitations

## Context

CIH supports microservice fleets via per-repo graphs + a "group" contract layer
(`group sync` → `contracts.jsonl` → `group_contracts`/`api_impact`/`shape_check`).
Four limitations were identified:

1. **Contract-site extraction is Java-only.** Only `java/parse/framework.rs` emits
   outbound HTTP/Kafka `contract_sites`; Kotlin/Python/TypeScript push `vec![]`
   (Python/TS do emit routes; Kotlin emits neither), Go emits neither routes nor calls.
2. **URL matching is literal-only.** `first_string_argument`
   (`java/parse/mod.rs:304`) accepts only `string_literal`; URLs built from constants
   or concatenation are dropped. `JavaConstantResolver` exists but is wired only into
   CALLS-edge enrichment (`cih-resolve/src/emit.rs:635`), not contract URLs.
3. **No cross-repo trace/impact.** `flow_downstream`/`impact`
   (`cih-falkor/src/lib.rs:797,315`) stop at `ExternalEndpoint`/`KafkaTopic` inside
   one graph; `api_impact` only echoes precomputed JSONL rows.
4. **`group sync` is manual and unstamped** — contracts silently go stale after
   re-analyzing a member repo.

**User decisions:** languages = Kotlin, TypeScript+Python, Go (no C#); cross-repo
architecture = **artifacts-based hop** (the `shape_check` pattern — no multi-graph
server, no merged graph).

First implementation step: copy this plan to `docs/plans/cross-repo-microservice-plan.md`
(repo convention) on the working branch.

---

## Phase A — Kotlin contract sites + routes (Spring/Feign/Kafka) — M

Kotlin Spring services are the biggest hole (invisible to `group sync` today).

- **Create** `crates/cih-lang/src/contracts_common.rs`: hoist pure string helpers out
  of Java — `rest_template_http_method`, `infer_webclient_http_method`,
  `normalize_external_url`, `base_type_simple`, Spring route-annotation→method table,
  `normalize_route_path`. Java `pub(crate) use`s them (no call-site churn).
- **Create** `crates/cih-lang/src/kotlin/framework.rs`: port Java's detection 1:1 —
  routes from `@GetMapping`/… + class-level `@RequestMapping` prefix
  (`Route:{METHOD path}` node, props `httpMethod`/`path`/`handler`, `HandlesRoute`
  edge — props shape must match; `load_repo_contracts` reads props, not id format);
  `@FeignClient` interfaces → `HttpClientProxy`; `@KafkaListener`/`@EventListener` →
  `EventListen`; `kafkaTemplate.send`/`publishEvent`/RestTemplate/WebClient `.uri`
  → `HttpCall`/`EventPublish`.
- **Modify** `crates/cih-lang/src/kotlin/parse.rs`: add a `callable_stack` to the
  builder (push in `emit_function_decl` ~:464), walk function bodies for
  `call_expression`/annotations to supply `ContractSite.in_callable`. Receiver typing
  via a light per-class env from primary-constructor params + typed
  `property_declaration`s (analog of `receiver_has_type`, `java/parse/mod.rs:330`).
- Do **not** share tree-walking with Java (grammars differ); share only string logic.
- Phase A accepts literal strings only (skip `${}` interpolation; Phase B upgrades).
- No resolve-side changes — `resolve_contract_edges` is language-agnostic.
- **Tests**: extend the existing `crates/cih-lang/tests/kotlin.rs` with an
  inline-source `contract_sites(src)` helper mirroring `java.rs:249` — RestTemplate,
  WebClient chain, Feign, KafkaListener, send, publishEvent, route with class prefix,
  Route node + HandlesRoute shape.

## Phase B — Dynamic-URL folding (constants + concat → `{*}` wildcards) — M

- **Modify** `crates/cih-core/src/ir.rs`: add
  `enum UrlPart { Lit(String), ConstRef(String), Dynamic }` and
  `ContractSite.url_parts: Option<Vec<UrlPart>>` with `#[serde(default)]`
  (old caches/artifacts must still deserialize). `url_template` stays for
  fully-literal URLs — zero behavior change there.
- **Parse side (Java, then Kotlin)**: new `url_argument_parts(node, src)` folding
  `+`-concat like `fold_string_init` does for SQL (`java/parse/constants.rs:177-218`):
  literal→`Lit`, identifier/field_access→`ConstRef`, else→`Dynamic`. Use at the
  RestTemplate (:182), WebClient `.uri` (:199), and KafkaTemplate topic (:214) sites.
  Kotlin: `"$base/items"` → `[ConstRef, Lit]`, `${expr}` → `Dynamic`; also emit
  `StringConstant`s from companion-object/`object` literals so the existing
  resolver index (built from every `ParsedFile` in
  `cih-resolve/src/constant_propagation.rs`) picks up Kotlin. Two Kotlin wrinkles
  Java doesn't have: (a) record `owner_fqcn` as the *referencable* name (`MyCls`,
  not `MyCls.Companion`) or the resolver's `(owner, name)` lookup misses; (b)
  top-level `const val` has no declaring class and bare-name references won't hit
  the owner-class-first lookup — **out of scope v1** (companion/`object` constants
  only; unresolved refs degrade to `{*}`, never wrong matches).
- **Resolve side**: fold in `resolve_contract_edges` (`cih-resolve/src/contracts.rs`)
  — pass the constant resolver in from `EdgeEmitter::run` (`emit.rs:158-160`);
  derive `ResolutionContext.owner_fqcn` from `in_callable`
  (`Method:pkg.Cls#m/2` → `pkg.Cls`) and imports from the `ParsedFile`, as
  `push_calls_edge` does (`emit.rs:649-665`). Unresolved parts → placeholder; any
  path segment containing a placeholder becomes `{*}` wholesale (never `v{*}`).
- **Guards**: skip emission if the result is `/` or all-`{*}`; set `dynamic: true`
  prop + small confidence discount. Matching stays normalized-string equality —
  `{*}` pairs only with provider path variables (`{id}`/`:id` → `{*}` via
  `normalize_contract_path`, `cih-core/src/group.rs:64-95`). Segment-wise true
  wildcard matching is an explicit non-goal.
- **Tests**: parser-level parts extraction (java.rs/kotlin.rs); resolve-level
  two-file fold (`static final BASE` + `getForObject(BASE + "/" + id)` →
  `/api/orders/{*}`, `dynamic:true`); `normalize_contract_path` idempotence on `{*}`.

## Phase C — TypeScript (fetch/axios) + Python (requests/httpx) outbound — M

- **Modify** `crates/cih-lang/src/typescript/parse.rs` and
  `crates/cih-lang/src/python/parse.rs` (contract_sites currently empty at
  ts:649/692, py:799/844).
- Tight recognizers to avoid false positives:
  TS — bare `fetch(url[, {method}])` (default GET), `axios.get|post|put|delete|patch|head`,
  `axios(url, {method})`; instance clients (`this.http.get`) out of scope v1.
  Python — module-receiver `requests.*`/`httpx.*` verb calls plus
  `requests.request("POST", url)`.
- URLs reuse Phase B parts: TS `template_string` / Python f-string → `Lit` +
  `Dynamic` per interpolation → `{*}` folding (ConstRef resolution mostly no-ops for
  TS/Py — fine).
- `in_callable`: use enclosing function id where tracked (Python threads `enclosing`,
  `python/parse.rs:459`); fall back to file id. Acceptable v1, but be precise about
  the cost: a file-id `in_callable` degrades the *first leg* of `trace_flow_x`
  (entry resolution), not just display granularity — Phase E tests must include a
  file-id-caller case so the behavior is pinned, not accidental.
- **Tests**: `contract_sites` helpers in `tests/typescript.rs` and
  `tests/python_parse.rs` incl. negative cases (`myobj.get(x)` not emitted).

## Phase D — Go routes (net/http, gin, echo, chi) + outbound — L

- **Create** `crates/cih-lang/src/go/framework.rs`; **modify** `go/parse.rs`
  (`parse_go_file:86`) to invoke it.
- Import-gated detection (no annotations in Go): gate on `RawImport` of `net/http`,
  gin, echo, chi, gorilla/mux; then shape-gate (verb-named method, first arg a string
  starting with `/`). Go 1.22 `http.HandleFunc("GET /orders/{id}", h)` → split
  method; otherwise method = `"ANY"`.
- **`ANY`-method matching**: `match_contracts` (`group_sync.rs:172`) keys providers
  on `(method, path)` — also probe `("ANY", path)` for consumers. Without this,
  net/http routes never match.
- Route node id: **decide the convention consciously.** Two already coexist —
  Express uses `Route:express:{METHOD}:{path}`, Java/Spring uses
  `Route:{METHOD} {path}`, and the CXF stitcher *rewrites* ids into the latter
  shape when it fires. Nothing parses id formats (props are the contract), so
  either is safe; default to `Route:go:{METHOD}:{path}` (Express precedent) and
  record the choice in ARCHITECTURE.md so the fork stays deliberate.
  Props `httpMethod`/`path`/`source`/`handler`;
  `HandlesRoute` only when handler is a plain identifier matching a same-file def.
  This is **new logic with no precedent to port**: Express
  (`typescript/parse.rs:367-391`) emits Route nodes with *no* handler edge, and
  NestJS/Spring resolve trivially because the decorated method *is* the handler.
  New `RouteSource` variants — grep exhaustive `match RouteSource::` first
  (clippy `-D warnings`).
- Outbound: `http.Get|Post|Head|PostForm`, `http.NewRequest(WithContext)` (method from
  literal arg 0; skip `client.Do`). URLs via parts: concat → `ConstRef`/`Dynamic`;
  `fmt.Sprintf` format-split on `%s|%d|%v` → `Dynamic`.
- **Tests**: new `crates/cih-lang/tests/go.rs` (inline-source); `ANY` matcher cases in
  `crates/cih-engine/tests/group_sync.rs`.

## Phase E — Cross-repo `trace_flow_x` + `api_impact` caller walk — L

- **Create** `crates/cih-server/src/xflow.rs`: pure core
  `ArtifactGraph { nodes_by_id, out_edges, in_edges }` from
  `load_artifact_nodes`/`load_artifact_edges` (`cih-server/src/utils.rs`; note
  `utils::resolve_repo` already exists there — moved from taint.rs for the wiki
  work). **Cache across calls, not per call**: fleet-member artifacts are big
  (Fineract nodes.jsonl = 87k nodes), so a 3-repo trace would otherwise pay
  seconds of jsonl parsing per invocation. Use the `WikiSearchState::get_or_load`
  pattern from `cih-server/src/wiki.rs` verbatim: an
  `Arc<RwLock<HashMap<artifacts_dir, Arc<ArtifactGraph>>>>` with file-mtime
  invalidation (keyed on nodes.jsonl mtime). BFS over
  `Calls|HandlesRoute|ExternalCall|PublishesEvent|ListensTo`
  (mirrors `flow_downstream`'s edge set).
- **New tool `trace_flow_x(entry_point, group, max_depth, max_hops)`** (not a param on
  `trace_flow` — keeps the existing `FlowHop` contract stable). Entirely
  artifacts-based, including the first leg (uniform semantics; hermetic tests);
  bound repo resolved via registry `graph_key == server graph_key`
  (`utils::resolve_repo`). Accepted trade-off: no Falkor `callSites` enrichment.
  - HTTP hop: `ExternalEndpoint` in repo R → `ContractMatch` rows
    (`kind==HttpRoute && consumer_repo==R && consumer_id==node.id`) → provider repo's
    Route node (`provider_id`) → **inverse** `HandlesRoute` (handler→route direction)
    → handler → downstream CALLS.
  - Event hop: `KafkaTopic` in R → rows (`provider_repo==R && match_key==topic`) →
    `consumer_id` listener in `consumer_repo`.
  - Budgets: per-repo depth default 6 (clamp 10), `max_hops` default 3, visited set
    keyed `(repo, node_id)`, node cap 200. Output steps carry `repo` and
    `via: {kind: CALLS|…|CONTRACT, match_key?}`. JSON only v1.
  - Failure modes: missing contracts.jsonl → same "run group sync first" error;
    missing provider artifacts → truncated hop marker, not a hard failure.
- **`api_impact` extension** (`cih-server/src/contracts.rs:43-84`): additive
  `include_callers`/`caller_depth` args (`#[serde(default)]`); per match, load
  consumer artifacts, collect `ExternalCall` edges with `dst == consumer_id`
  (same index `shape_check` builds at :159-166), reverse-`Calls` BFS to depth
  (default 3), attach enclosing route via `HandlesRoute`. New field
  `consumer_callers: [{method_id, route?}]`.
- **Modify** `crates/cih-server/src/args.rs` (new/extended arg structs) and `app.rs`
  (register tool next to `trace_flow`, now at :373 — cih-server line refs in this
  plan predate the wiki-search commits; re-grep rather than trusting offsets).
- **Provider-row expectations**: since the CXF dual-server route cloning
  (dev@5c7b1f3), OSGi-style providers emit TWO Route nodes per operation
  (`/v1` + `/ns/v1`). `load_repo_contracts` picks up both, so
  `group_contracts`/`api_impact` rows can legitimately double for such providers —
  E's tests must not assume one provider row per endpoint. (Upside: the same fix
  is what makes OCB providers matchable at all — pre-fix their routes were bare
  local paths that could never equal a consumer URL.)
- **Tests**: pure in-memory two-"repo" fixtures + `Vec<ContractMatch>` in xflow
  `#[cfg(test)]` (hop discovery both directions, inverse-HandlesRoute, budgets);
  tempdir jsonl fixtures for `ArtifactGraph` loading. No test reads real `~/.cih`.

## Phase F — Auto group sync + freshness stamps — M

- **Hook point**: `persist_analyze` (`cih-engine/src/registry.rs:45`) and
  `persist_discover` (:55) — funnels for `analyze`, `discover`, **and** `refresh`
  (which calls both; no refresh.rs changes). Hook calls
  `auto_sync_groups_for_repo(&entry.name)`: load `GroupRegistry`, new
  `groups_containing(name)` helper (`cih-core/src/group.rs`), run existing
  `sync_group` per group; `tracing::warn!` on error, **never propagate** (analyze
  must not fail on a sibling repo's missing artifacts).
- **Layering**: `registry.rs` is lib-layer; don't call into `cmd/` from it. Hoist
  the sync core (`sync_group`, `match_contracts`, `load_repo_contracts`) out of
  `cmd/group_sync.rs` into a non-cmd module (e.g. `cih-engine/src/group_sync.rs`),
  leaving `cmd/group_sync.rs` a thin shim — same pattern as the binaries.
- **Escape hatch**: `CIH_NO_AUTO_GROUP_SYNC=1`, env-only for v1. If per-repo config
  is ever needed, promote to a settings-schema option (clap `Option<T>` flag +
  `resolve_*` in `settings.rs`) per the repo's layered-config convention.
- **Cost**: `sync_group` re-reads `nodes.jsonl`/`edges.jsonl` of *every* member repo
  — O(fleet) I/O per analyze, per containing group. Believed acceptable for small
  fleets, but per the repo's measure-first discipline: **capture a wall-time number
  on a 3-repo group in this phase's verification** instead of assuming; the
  sync-state stamp (below) gives a future cheap skip-if-unchanged path, not v1.
- **Stamp = separate file** `~/.cih/groups/<name>/sync-state.json` (a header line in
  contracts.jsonl would break old strict-parsing servers):
  `{ synced_at, generation, repos: [{name, indexed_at, last_git_head}] }` — snapshot
  of each member `RegistryEntry` at sync time; atomic tmp+rename write (mirror
  `RefreshState::save`, `refresh.rs:38-45`); all fields `#[serde(default)]`.
- **Staleness predicate** (pure fn in cih-core next to `Registry::is_stale`,
  `registry.rs:168`): stale iff any member missing from registry, or its
  `indexed_at`/`last_git_head` differs from the snapshot, or contracts exist unstamped.
- **Surfacing**: `status` tool (`app.rs:296`) gains
  `groups: [{name, contracts_synced_at, stale}]`; contract tools + `trace_flow_x`
  responses gain `contracts_synced_at`/`contracts_stale` (all additive JSON).
  Optional `cih-engine group status <name>` subcommand.
- Hermeticity: keep `auto_sync_groups_for_repo` parameterized over `&GroupRegistry`;
  in tests with isolated HOME, absent groups.json → no-op.
- **Tests**: `cih-core/tests/group.rs` (SyncState roundtrip, staleness cases);
  `cih-engine/tests/group_sync.rs` (selection, stamp written, failure swallowed).

---

## Cross-cutting risks

| Risk | Mitigation |
|---|---|
| `{*}` over-wildcarding → false matches | Equality matching bounds it to provider path-variable segments; skip all-wildcard paths; `dynamic:true` + confidence discount |
| Schema compat (contracts.jsonl, artifacts, parse cache) | `ContractMatch` untouched; `url_parts`/`SyncState` are `#[serde(default)]`; stamp is a separate file |
| Go `ANY` changes matcher behavior | Isolated to `match_contracts`; covered by in-memory tests |
| Kotlin/Go grammar node-kind assumptions | Write inline-source tests first per pattern |
| clippy `-D warnings` / fmt gate | Grep exhaustive `RouteSource` matches before adding variants; snake_case serde consistency |
| Hermetic workspace tests | Pure-core vs thin-glue split; tempdir HOME for anything touching `~/.cih` |
| Stale line refs (cih-server) | Wiki-search commits shifted `app.rs`/`utils.rs`; re-grep every `:NNN` in this plan at implementation time |
| CXF dual-server cloning doubles provider Route rows | Expected since dev@5c7b1f3; E/F tests and any route-count dashboards must not assume one row per endpoint |

## Documentation (repo convention — lands with each phase)

- **`docs/ARCHITECTURE.md`** gets each phase's new parser assumptions / known limits:
  Kotlin receiver-typing heuristic + companion-only constant scope (A/B), `{*}`
  folding semantics and its matching bounds (B), TS/Python recognizer scope — no
  instance clients, module-receiver only (C), Go import+shape gating and `ANY`
  semantics (D).
- **`CLAUDE.md` tool table** + `docs/agent-workflows/` gain `trace_flow_x` and the
  `api_impact` caller-walk args (E), and the group-freshness fields on `status` (F).

A → B → C → D → E → F. Only B has cross-phase coupling (C/D emit `url_parts`);
E and F are independent and can be pulled earlier if desired. **Recommended: pull
F first** — it is the smallest phase, fixes a today-problem (contracts silently
going stale; a real group already exists in `~/.cih/groups/`), and its freshness
stamps make every later phase's end-to-end verification trustworthy. Each phase
lands as its own commit(s) on a feature branch off `dev`.

## Verification

Per phase: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` (hermetic — no FalkorDB needed).

End-to-end (after C/E/F, on the dev machine with FalkorDB on 6380):
1. `cih-engine analyze` two small polyglot fixture services (e.g. a Kotlin-Spring
   provider + a TS consumer calling it via `fetch`), `group create/add` both,
   confirm auto-sync fires on the second analyze (Phase F) and
   `~/.cih/groups/<g>/contracts.jsonl` + `sync-state.json` appear.
2. Via MCP: `group_contracts` shows the HTTP match; `api_impact(include_callers=true)`
   lists the TS caller; `trace_flow_x` on the consumer's route crosses into the
   provider repo (step with `via.kind == "CONTRACT"`).
3. Re-analyze the provider with a changed route → `status` reports the group stale
   until sync re-runs, then fresh.

## Review log

- 2026-07-11: reviewed against dev@2534c7d; all load-bearing claims verified in
  source (`first_string_argument` literal-only at mod.rs:304; TS/Py/Kotlin
  contract_sites empty; Express emits no HandlesRoute while NestJS does;
  `match_contracts` keyed on (method,path); `persist_analyze`/`persist_discover`
  at registry.rs:45/:55; falkor impact/flow_downstream at :315/:797; nothing
  parses `Route:` id formats). Accepted amendments folded above: (1) cross-call
  mtime-cached `ArtifactGraph` in Phase E (WikiSearchState pattern) instead of
  "cached per call"; (2) Go route-id convention made an explicit documented
  decision (Express vs Spring format fork is pre-existing); (3) Phase C file-id
  `in_callable` fallback degrades trace_flow_x entry resolution — pinned by test;
  (4) Phase F O(fleet) sync cost must be measured, not assumed; (5) cih-server
  line refs stale after the wiki-search commits — re-grep at implementation;
  (6) CXF dual-server cloning (dev@5c7b1f3) doubles provider Route rows and is
  also the prerequisite that makes OSGi providers matchable cross-repo at all.
  Sequencing: F recommended first (independent, smallest, fixes a live problem).
