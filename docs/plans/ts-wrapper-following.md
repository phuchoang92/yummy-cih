# TS HTTP wrapper following: apiFetch-style call sites become contract endpoints

> **STATUS: COMPLETED 2026-07-11** — commits `01141ec` (IR plumbing, output-neutral), `ef0367e` (TS detection + provisional sites, schema→4), `46b44a4` (two-context resolve join), docs in the same push.
> **Live results (212ecom):** ExternalEndpoints **1 → 163** (162 via wrapper, all `/api/v1` bases with `via_wrapper` + `base_source: env_default` provenance); cross-repo HTTP matches **1 → 140**. servicemix/fineract byte-identical (TS-only feature). 104 workspace suites green, clippy clean.


## Context

The motivating gap is quantified on the user's own stack: `212ecom-fe` routes HTTP through a wrapper — **156 `apiFetch(...)` call sites vs 1 bare `fetch`** — so today exactly one `ExternalEndpoint` is extracted and cross-repo matching sees ~0.6% of the real API surface. The wrapper (`src/services/apiClient.ts:28-41`) is the canonical shape: `apiFetch(endpoint, options?, token?)` builds `const url = \`${API_BASE_URL}${endpoint}\`` (local-var indirection; the `fetch(url,…)` sits inside a `try`) and callers pass relative literals/templates plus an options object with optional `method`. Combined with the already-landed constant folding (`API_BASE_URL → '/api/v1'` env-default), following the wrapper turns those 156 sites into matchable `/api/v1/...` endpoints.

Design: detect same-repo wrapper DEFs at parse time; emit provisional call sites (`via_wrapper`) for URL-ish calls to plain identifiers; join at resolve with a **two-context fold** (wrapper's prefix parts resolve in the wrapper file's context, caller's suffix in the caller's); unmatched provisionals drop silently. Python analog deferred; IR shaped language-neutrally.

**Verified anchors** (fresh): identifier arm allow-list at `typescript/parse.rs:492-496` (`apiFetch` callee IS an identifier hitting `_ => return`); `call_options_method` :557 (arg 1 — matches v1); `fold_ts_url_expr` :603 (lowercase `${endpoint}` → Dynamic under the SCREAMING_SNAKE gate → wrapper detection matches the param NAME, not parts); TS walker has NO `arrow_function` arm — hook `lexical_declaration → variable_declarator → arrow_function` + `function_declaration`; indirection must scan the whole body (fetch nested in `try`); `resolve_contract_edges(parsed, resolver)` sees all files (`cih-resolve/src/contracts.rs:19`); `resolve_relative_module`/`strip_source_extension` are module-private in `java/constant_resolver.rs:84/:111` — promote; `group_sync`/wiki/taint never read `contract_sites` (dropped provisionals invisible); constructor churn: ContractSite 27 literals, ParsedFile ~53 (mechanical; string_constants precedent); PARSE_CACHE_SCHEMA=3, GOLDEN=(3,"d1b86459df44723d"); 212ecom-fe importers all use relative specifiers.

Branch `dev`. At implementation start copy this plan to `/Users/phuc/BigMoves/AI/ts-wrapper-following-plan.md` and `docs/plans/ts-wrapper-following.md` (committed with Commit 1).

## Commit 1 — `feat(ir): HttpWrapperDef, ContractSite.via_wrapper, ParsedFile.http_wrappers plumbing` (output-neutral)

- **`cih-core/src/ir.rs`**: new `HttpWrapperDef { name, module, prefix_parts: Vec<UrlPart>, options_arg_index: u32, range }` (doc: v1 prefix = Lit/ConstRef only, options at arg 1). `ContractSite` gains `#[serde(default, skip_serializing_if = "Option::is_none")] pub via_wrapper: Option<String>`; `ParsedFile` gains `#[serde(default, skip_serializing_if = "Vec::is_empty")] pub http_wrappers: Vec<HttpWrapperDef>`. The `skip_serializing_if`s are **load-bearing**: serialized output stays byte-identical for non-wrapper code, so this commit needs NO schema bump and the guard stays green.
- Fix the 27 + ~53 literal constructors (`via_wrapper: None`, `http_wrappers: Vec::new()`).
- **Helper promotion**: move `resolve_relative_module` + `strip_source_extension` from `java/constant_resolver.rs` into `cih-lang/src/constant_resolver.rs` as `pub fn`, re-export from `cih-lang/src/lib.rs` (precedent: `normalize_external_url`); Java resolver imports from the new home; cih-resolve consumes via `cih_lang::…`.
- Gate: workspace compiles, all tests pass, `parse_schema_guard` green (proving neutrality).

## Commit 2 — `feat(typescript): detect HTTP wrapper defs + provisional wrapper call sites`

### Wrapper-def detection (`typescript/parse.rs`)
`Builder` gains `http_wrappers`; plumb into both ParsedFile constructions (:919/:962). Hooks: `function_declaration` arm (module scope) and `lexical_declaration` arm (module scope, declarator value = `arrow_function`) call `try_collect_http_wrapper(name, fn_node, src, builder)`:
1. First param must be an `identifier` pattern (typed params still are; destructuring → bail).
2. Recursive body scan for the FIRST `call_expression` with callee identifier `fetch`/`axios` or `axios.<verb>` (reuse `axios_http_verb` :470); do NOT descend into nested function definitions (closures must not fake wrappers). None → bail.
3. URL expr = arg 0. If `identifier`: one-level indirection — whole-body scan for `const` declarators named it with a value; **exactly one** → its value becomes the URL expr; else bail. (Covers `const url = …` at body top with `fetch(url,…)` inside `try`.)
4. Param-aware fold into local `enum WrapperUrlPiece { Part(UrlPart), Param }`: mirror `fold_ts_url_expr` but a substitution/identifier whose text == the first-param name → `Param`.
5. Validate: LAST piece is `Param`, no other `Param`, all earlier pieces `Lit`/`ConstRef` (any `Dynamic` → bail). Push `HttpWrapperDef { name, module: builder.module, prefix_parts, options_arg_index: 1, range }`. (apiClient → `prefix_parts=[ConstRef("API_BASE_URL")]`; the env-default constant is already extracted.)

### Provisional call sites
In `try_emit_http_contract`'s identifier arm, replace `_ => return`: for non-allow-listed callees, when **arg 0 is URL-ish** (string starting `/`, template whose first fragment starts `/`, or concat with first Lit starting `/`): push `ContractSite { kind: HttpCall, via_wrapper: Some(callee), url_parts: Some(parts — always parts, even for plain literals, since resolve must prepend the prefix; bypass the "must contain non-Lit" filter), url_template: None, http_method: call_options_method(...).unwrap_or GET, in_callable, range }`. `fetch`/`axios` names hit the allow-list first (colliding wrapper name silently ignored — document). Member-expression callees untouched → `instance_clients_are_not_emitted` stays green. Volume: `navigate('/x')`-style false provisionals drop at resolve; ~100 bytes each in parse cache; acceptable.

### Schema + guard
Bump `PARSE_CACHE_SCHEMA` → 4; extend the guard corpus with the apiClient-shaped wrapper + a caller file; run guard once to print the hash → `GOLDEN = (4, "<printed>")` (one combined update).

### Parser tests (`cih-lang/tests/typescript.rs`)
Wrapper detected via local-var indirection (arrow + function forms); rejections (param mid-URL, no inner fetch, member-callee inner call, destructured param, ambiguous `const url`); provisional emission for `/x` literal (POST from options) and template (`[Lit, Dynamic]`, GET); negatives (`t('common.title')`, `helper(id)` → nothing); regressions pinned (`instance_clients_are_not_emitted`; the wrapper's own inner `fetch(url,…)` folds to all-`{*}` and drops — pin it).

## Commit 3 — `feat(resolve): join wrapper call sites with two-context URL folding`

`cih-resolve/src/contracts.rs`:
- Behavior-preserving refactor: split `fold_parts_raw` into ctx-taking `fold_url_parts(parts, ctx, resolver) -> FoldedParts` + the site-level ctx builder; extract `fold_http_url`'s normalize/`{*}`-segment tail into `wildcard_segments(raw, env_default) -> Option<(String, bool)>`.
- Pre-pass `WrapperIndex` over all parsed files: `by_key: (module, name) → (&HttpWrapperDef, &ParsedFile)` + `unique_by_name` (2+ → None, never guess).
- In the HttpCall branch, when `site.via_wrapper = Some(name)`: resolve wrapper by (a) same module (`strip_source_extension(pf.file)`), (b) import-scoped (`resolve_relative_module` per non-static import), (c) unique-by-name. No match → `continue` (site vanishes — no node, no edge).
- **Two-context fold**: prefix folded with `ResolutionContext { file: wrapper_pf.file, owner_fqcn: wrapper.module, imports: wrapper_pf.imports, allow_unique_fallback: true }`; suffix with the existing caller ctx; concatenate raws; `wildcard_segments(normalize_external_url(raw), prefix.env_default || suffix.env_default)`; emit with `dynamic=true` confidence, `props.via_wrapper = "<module>#<name>"`, existing `base_source` provenance.

### Resolve tests (`cih-resolve/tests/resolve.rs`)
Happy path (`POST /api/v1/admin/x`, `via_wrapper` + `base_source` props, ExternalCall edge); **two-context proof** (second same-named constant elsewhere kills unique fallback, caller imports nothing → still resolves via the wrapper file's context); unmatched provisional → zero output; import-scoped `'../services/apiClient'`; ambiguous wrapper without import → dropped.

## Commit 4 — `docs(architecture): document TS HTTP wrapper following`
ARCHITECTURE.md "Dynamic-URL folding" section gains a wrapper subsection: detection rules (param-last, one-level indirection, no closures), drop-on-no-match, v1 limits (barrel re-exports, `new URL()`, axios.create instances, non-arg-1 options, Python deferred — IR is language-neutral).

## Verification

Per commit: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` (hermetic).
Live, after all commits (release build):
1. `cih-engine analyze /Users/phuc/BigMoves/dienmaychiben/212ecom-fe --all --no-load` (schema bump auto-invalidates cache) → count ExternalEndpoints in latest artifacts: expect **~1 → tens-to-~150** distinct `/api/v1/...` endpoints (156 sites, dedup + `{*}` collapse), spot-check `via_wrapper` + `base_source` props.
2. Recreate the throwaway fe+be group, `group sync` → HttpRoute match count **> 1** (was exactly 1); `api_impact` on a matched backend route lists fe consumers; remove the group after.
3. Regression: servicemix/fineract eval — byte-identical routes/edge-reasons (feature is TS-only).

## Risks — resolved in design

| Risk | Resolution |
|---|---|
| Provisional false positives (`navigate('/x')`) | Dropped at resolve; never reach artifacts (verified: only resolve_contract_edges consumes contract_sites) |
| Wrapper name via alias/barrel import | unique_by_name fallback (exactly-one) rescues; ambiguity → drop, never a wrong endpoint; documented v1 limit |
| Commit-1 output drift breaking the guard | `skip_serializing_if` on both fields keeps serialization byte-identical; guard run proves it |
| Wrapper's constants invisible to callers | Two-context fold resolves prefix in the wrapper file's own context — pinned by a dedicated test |
| Closures inside wrapper faking detection | Body scan does not descend into nested function definitions |
