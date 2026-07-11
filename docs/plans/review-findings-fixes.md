# Fix the three cross-repo review findings: parse-cache versioning, TS/Py URL constants, trace_flow_x repo override

> **STATUS: COMPLETED 2026-07-11** — all three findings fixed, risk resolutions included, landed on `dev`.
>
> | Finding | Commit | Live proof |
> |---|---|---|
> | F1 parse-cache versioning + schema guard | `e49ded8` | cached analyze (no --no-cache) now picks up new extraction; parse-cache/v3 only, legacy pruned; guard caught BOTH parser-output changes in this branch as designed |
> | F3 trace_flow_x repo override + validation | `bbff6b4` | MCP: repo="212ecom-be" traces in be; non-member rejected naming members; api_impact lists the fe caller |
> | F2 TS/Py URL constants + gated fallback | `44f57d6` | fe endpoint folds to literal `GET /api/v1/admin/notifications/unread-count` with `base_source: env_default` → **first real fe→be contract match (1 HttpRoute)**; schema bumped to 3 |
>
> Added during implementation (guard-driven): ConstRef gating on SCREAMING_SNAKE identifiers — params/locals (`${id}`) stay Dynamic so the
> cross-file fallback can never see them; the resolver's import-scoped and unique steps enforce the same convention.
>
> **Verification:** 104 workspace test suites green, clippy clean; servicemix byte-identical; fineract routes/reasons identical except
> 3 additive `event-publish` edges attributable to the pre-existing Phase-B Java topic folding (baseline predated it) — inspected and legitimate.

## Context

The code review of the cross-repo microservice implementation (dev@`b3cbbba`) proved three defects live:

1. **Parse cache is version-blind (HIGH)** — cached `ParsedUnit`s are keyed on `blake3(file bytes)` only (`cih-engine/src/file_cache.rs:65-110`, flat `parse-cache/<hash>.json`, no GC). Proven: re-analyzing 212ecom-fe served pre-Phase-C ParsedFiles → 0 ExternalEndpoints vs 1 with `--no-cache`. Every parser upgrade silently no-ops on unchanged files.
2. **TS/Py URL constants fold to `{*}` → zero real matches (HIGH)** — TS `${…}` substitutions are `UrlPart::Dynamic` unconditionally (`typescript/parse.rs:611`); TS/Py emit no `string_constants`; the resolver has no cross-file fallback. Proven on real repos: fe `fetch(`​`${API_BASE_URL}/admin/notifications/unread-count`​`)` with `API_BASE_URL = import.meta.env.VITE_API_URL ?? '/api/v1'` vs be `GET /api/v1/admin/notifications/unread-count` → **0 contract matches**.
3. **trace_flow_x has no repo override (MEDIUM)** — start repo = FIRST registry entry with the server's graph_key (`cih-server/src/contracts.rs:218-229`); broke live on a stale entry; no group-membership validation.

**User decisions**: F1 = manual schema const (no auto build-id). F2 = literal consts + env-default heuristic (`?? / || / or / os.environ.get(k, d)` → literal default), NO suffix matching. F3 = house-convention `repo` arg + membership validation.

Commit order **F1 → F3 → F2** (small/urgent first), conventional commits, one per finding, on `dev`. At implementation start, copy this plan to `/Users/phuc/BigMoves/AI/review-findings-fixes-plan.md` and `docs/plans/review-findings-fixes.md` (committed with F1).

**Verified anchors**: `prune_other_versions(parent, keep)` exists (`versioning.rs:63`, prunes dirs only, best-effort). Exactly TWO `ResolutionContext` construction sites: `cih-resolve/src/contracts.rs:202` and `cih-resolve/src/emit.rs:653`. `analyze_config_fingerprint` at `analyze/mod.rs:607-616` (persisted `analyze-config.json`, compared in `config_unchanged` `cache.rs:234-245`). `ParsedFile.language` set from `provider.language_id()` (`"typescript"`, `"python"`). `normalize_external_url` strips scheme+host (`contracts_common.rs:66-82`). `build_java_constant_resolver` already collects `string_constants` from every ParsedFile language-agnostically.

## Commit 1 — `fix(cache): version the parse cache by parser schema`

- **`crates/cih-lang/src/lib.rs`**: `pub const PARSE_CACHE_SCHEMA: u32 = 2;` with a doc comment stating the bump rule ("bump whenever any parser/extractor changes the shape OR content of ParsedUnit output"). Starts at 2 — the flat pre-versioning era is implicitly v1 and never collides.
- **`crates/cih-engine/src/file_cache.rs`**: `cache_path`/`load_cached_parsed`/`save_cached_parsed` gain a `schema: u32` **parameter** (tests vary it without bumping the const); path becomes `<cih_dir>/parse-cache/v<N>/<hash>.json`. New `pub fn prepare_parse_cache(cih_dir, schema)`: create `v<N>/`, `prune_other_versions(parse-cache/, "v<N>")`, plus best-effort removal of legacy **flat** `*.json` files (prune helper skips non-dirs) — the migration path.
- **`crates/cih-engine/src/analyze/cache.rs`** `parse_scope` (:43): call `prepare_parse_cache` once after `hash_all`, **before both branches** (the `--no-cache` branch at :57-83 also writes). Thread `cih_lang::PARSE_CACHE_SCHEMA` through the 3 call sites (:62, :140/:144, :195).
- **`crates/cih-engine/src/analyze/mod.rs`**: fold the schema into `analyze_config_fingerprint` via a testable `analyze_config_fingerprint_with(.., schema)` split — a bump also disqualifies whole-repo no-op reuse. (One forced re-resolve per repo after upgrade — desired.)
- **Docs**: CLAUDE.md bump rule; ARCHITECTURE.md new "Parse cache" subsection (layout, invalidation = bytes-hash × schema, pruning).
- **Risk resolution — bump discipline becomes CI-enforced**: new `crates/cih-engine/tests/parse_schema_guard.rs` — a golden-corpus guard. Inline fixture sources exercising every extractor family (Java route+contract site, Kotlin route, TS fetch+const, Python request+const, Go route) are parsed; the resulting `ParsedUnit`s are serialized to canonical JSON and blake3-hashed; the test asserts the hash equals a `GOLDEN` const **paired with the schema number** (`const GOLDEN: (u32, &str) = (PARSE_CACHE_SCHEMA, "<hash>")`), with assert messages that spell out the required action: "parser output changed — bump cih_lang::PARSE_CACHE_SCHEMA and update GOLDEN". Any parser-output change now FAILS CI until both are updated together — the manual const is no longer habit-dependent. (Determinism: serde_json's default map is sorted; node order is parse order; if flakiness appears, fall back to comparing structural counts per fixture instead of one hash.)
- **Risk resolution — migration observability**: `prepare_parse_cache` logs `tracing::info!` when it pruned anything: "parse cache schema v<N> — stale cache cleared, this run re-parses all files" so the one-time slow analyze is explained, not mysterious.
- **Tests** (`cih-engine/tests/file_cache.rs` + a fingerprint unit test): `cache_path_is_versioned`, `load_misses_other_schema` (schema-bump-forces-reparse, tested by varying the param), `prepare_prunes_stale_versions_and_flat_legacy`, `fingerprint_varies_with_parse_schema`, plus the schema guard above.

## Commit 2 — `fix(server): trace_flow_x repo override + group-membership validation`

- **`crates/cih-server/src/args.rs`** `TraceFlowXArgs`: add house-convention field — `/// Repo name or absolute path (from registry) to start the trace in. Leave empty to use the server's active graph key. Must be a member of `group`.` `#[serde(default)] pub repo: String`. Update `entry_point` doc.
- **`crates/cih-server/src/utils.rs`**: new `pub fn resolve_repo_entry(repo, graph_key) -> Result<RegistryEntry, String>` holding the current `resolve_repo` body (returns `entry.clone()`); `resolve_repo` becomes a one-line wrapper (its two callers, wiki.rs/taint.rs, untouched).
- **`crates/cih-server/src/contracts.rs`** `trace_flow_x`: replace the inline first-match lookup with `resolve_repo_entry(&args.repo, graph_key)` → invalid_params on error. Add pure helper `validate_group_member(group, members, repo_name) -> Result<(), String>`; call with `GroupRegistry::load().find(&args.group)` members (missing group → invalid_params "group not found — run cih-engine group create/sync"; non-member → invalid_params naming all members). Missing-artifacts error becomes `invalid_params` with a "re-run `cih-engine analyze <repo>`" hint. Update the tool description in `app.rs` (~:388).
- **Tests**: args serde default (`tests/args.rs`); `validate_group_member` accept/reject with member names in the message (pure `#[cfg(test)]` — the full handler reads `~/.cih`, deliberately not integration-tested).

## Commit 3 — `feat(contracts): fold TS/Py URL constants (literal + env-default) with gated unique-name fallback`

### Parse side
- **TS `typescript/parse.rs`** — `fold_ts_url_expr` template arm (:611): descend into `template_substitution`; bare `identifier` → `UrlPart::ConstRef(text)`, anything else (member_expression, calls) stays `Dynamic` (**identifier-only v1** — property names would poison the unique fallback). Fix the stale ":588 resolution is a no-op" doc.
- **TS constants**: `Builder` gains `string_constants`; new `lexical_declaration` walk arm at **module scope only** (`class_fqn.is_none() && enclosing_fn.is_none()`; existing `export_statement` arm recurses so `export const` is covered), `const` only: plain `string` initializer → `StringConstant { const_name, owner_fqcn: module_path_without_extension, value, dynamic: false }` (e.g. owner `src/services/apiClient` — round-trips with `owner_fqcn_of` for same-module sites); `binary_expression` with `??`/`||` and string-literal **right** operand → the literal default; anything else emits nothing. Plumb into `ParsedFile` (:889; :846 failure path stays empty).
- **Python `python/parse.rs`** — f-string `interpolation` arm (:920): bare `identifier` → ConstRef, `attribute` stays Dynamic. Module-scope `assignment` arm: plain string → constant (dotted-module owner per `module_path`); `boolean_operator` `or` + literal right → default; `os.environ.get(k, "lit")` / `os.getenv(k, "lit")` → the literal second arg. Plumb into `ParsedFile` (:1027).

### Resolver
- **`cih-lang/src/constant_resolver.rs`**: two contained trait changes (exactly 2 impls — `JavaConstantResolver`, `NullConstantResolver` — and 2 call sites — `contracts.rs:202`, `emit.rs:653` — all verified):
  1. `ResolutionContext` gains `pub allow_unique_fallback: bool`. Set `contracts.rs` → `matches!(pf.language.as_str(), "typescript" | "python")`; `emit.rs` → `false` (**Java/Kotlin arg folding byte-identical**).
  2. **Risk resolution — env-default provenance**: `resolve` returns `Option<ResolvedConstant>` where `pub struct ResolvedConstant { pub value: String, pub env_default: bool }`. `StringConstant` (ir.rs) gains `#[serde(default)] pub env_default: bool` (serde-compatible; stale parse caches are moot — this commit bumps the schema). `fold_parts_raw` bubbles `any(env_default)` up so the emitted `ExternalEndpoint` carries a `"base_source": "env_default"` prop — consumers can see the approximation. The existing `dynamic: true` + confidence discount already applies to all folded URLs; no further discount.
- **`cih-lang/src/java/constant_resolver.rs`** — bare-name miss path (owner/static-import/super all missed), only when `ctx.allow_unique_fallback`:
  1. Retry `(owner_with_.ts/.tsx/.py_stripped, name)` — module-scope sites carry `in_callable = File:src/x.ts` so owners mismatch by extension.
  2. **Risk resolution — import-scoped lookup before any blind fallback**: for each non-static `ctx.imports` raw, derive the candidate owner — TS relative paths (`./apiClient`, `../x`) normalized against `ctx.file`'s dir into the repo-relative extensionless owner scheme; Python dotted raws used as-is — and look up `(candidate_owner, name)`. A hit here is *scoped* resolution: the site's file actually imports the constant's module, so same-named constants in unrelated/dead files can no longer be picked up. Needs a ~20-line `resolve_relative_module` helper **in cih-lang** (cih-resolve's `resolve_relative` can't be used — wrong dependency direction) with its own unit tests (`./x`, `../x`, `.` collapsing).
  3. Only then the repo-wide `unique_by_name` map (`HashMap<String, Option<String>>`: exactly one non-dynamic constant with that name → `Some`; 2+ → `None`, never guess) — now a last resort covering barrel re-exports, with the collision window shrunk to files that both fail import-scoping AND are repo-unique.
  Java sites never reach any of this (gate false) — isolation is structural.
- **No new wiring**: constants flow into `build_java_constant_resolver` automatically; `normalize_external_url` already handles absolute resolved bases.
- **Schema bump dogfood**: this commit changes parser output (new `string_constants`, ConstRef parts), so it must bump `PARSE_CACHE_SCHEMA` to 3 and update the guard-test `GOLDEN` — the first real exercise of Commit 1's enforcement.

### Tests
- `cih-lang/tests/typescript.rs`: `${API_BASE_URL}/admin/x` → `[ConstRef, Lit]`; `${cfg.base}` → Dynamic; constant emission for `?? '/api/v1'`, `|| form`, plain literal; negatives (`const X = getBase()`, `let`, function-body const → nothing).
- `cih-lang/tests/python_parse.rs`: f-string ConstRef vs attribute-Dynamic; constants for plain / `or` / `environ.get` / `getenv`; negatives (computed RHS, inside `def`).
- `cih-resolve/tests/resolve.rs` (extend the Phase-B folding section): **script e2e** (TS apiClient const + TS service site → `ExternalEndpoint GET /api/v1/admin/x`); **import-scoped beats ambiguity** (two same-named constants in different files, the site's file imports one of them → THAT one resolves, no `{*}`); **unscoped ambiguity** (two same-named constants, no import path → `{*}` base, never a guess); **Java ungated isolation pin** (java-language site + unique repo-wide constant with no import path → still wildcards, identical to today); same-file `File:`-owner extension-strip resolution; **env-default provenance** (endpoint from a `?? '/api/v1'` constant carries `base_source: "env_default"`).
- Optional: `cih-engine/tests/group_sync.rs` row pinning the matched key shape `GET /api/v1/admin/x`.

### Docs
ARCHITECTURE.md: TS/Py constant scope (module-level only, literal or env-default initializers), env-default semantics (folded path reflects the code default; prod env overrides are invisible — documented approximation), unique-fallback rule (script sites only, exactly-one candidate, 2+ → `{*}`, Java/Kotlin unchanged).

## Verification

Per commit: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` (hermetic; tempdirs for cache tests).

End-to-end on this machine after all three:
1. **F1 live**: rebuild; `analyze` 212ecom-fe **without** `--no-cache` → `parse-cache/v2/` exists, flat legacy files pruned, endpoint count equals the `--no-cache` run (≥1 incl. `unread-count`); second analyze still no-op reuses.
2. **F2 live**: analyze 212ecom-be; `group sync` a two-repo group → **≥1 HttpRoute match** fe→be; `api_impact("GET","/api/v1/admin/notifications/unread-count")` lists the fe consumer; the fe `ExternalEndpoint` node carries `base_source: "env_default"` (provenance visible in artifacts). (Recreate a throwaway group; remove it after.)
3. **Regression**: servicemix + fineract eval → routes/edge-reasons byte-identical (Java/Kotlin isolation).
4. **F3 live**: MCP `trace_flow_x(repo="212ecom-be", group=…)` starts in be; non-member repo → invalid_params naming members; empty repo → old behavior.

## Risks — each actively resolved in the commits above

| Risk | Resolution (not just mitigation) |
|---|---|
| Java resolver behavior drift | Structurally impossible: `allow_unique_fallback` is `false` at emit.rs:653 and gated to typescript/python at contracts.rs:202; pinned by the Java-ungated test; double-checked by servicemix/fineract eval byte-identity |
| Schema-bump discipline (manual const forgotten) | **CI-enforced**: the golden-corpus `parse_schema_guard` test fails on any parser-output change until `PARSE_CACHE_SCHEMA` + `GOLDEN` are updated together; Commit 3 itself dogfoods the bump (schema → 3) |
| Cache migration surprises | One-time cold reparse is announced via `tracing::info!` in `prepare_parse_cache`; legacy flat files are actively pruned; the `--no-cache` branch also calls prepare (covered by test) |
| Env-default value wrong in prod | Provenance is carried, not hidden: `ResolvedConstant.env_default` → `base_source: "env_default"` prop on the endpoint, on top of the existing `dynamic: true` + confidence discount; documented in ARCHITECTURE.md |
| Unique-fallback picks a dead-code constant | Import-scoped lookup runs FIRST (the site's file must import the constant's module), shrinking the blind fallback to barrel re-exports; ambiguity still degrades to `{*}`, never guesses — pinned by the import-scoped-beats-ambiguity test |

Residual (accepted, documented): the env-default literal reflects the code default rather than a prod env override — inherent to static analysis; visible via the provenance prop.
