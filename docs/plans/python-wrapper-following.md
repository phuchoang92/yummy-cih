# Python HTTP wrapper following (analog of the TS feature) + dotted-import bugfix

## Context

The TS wrapper feature (commits `01141ec`/`ef0367e`/`46b44a4`) took 212ecom-fe from 1 → 163 endpoints and 1 → 140 cross-repo matches. Python services use the same pattern (`def api_get(path): url = f"{API_BASE}{path}"; return requests.get(url)`), and the IR (`HttpWrapperDef`, `via_wrapper`, `http_wrappers`) plus the resolve-side `WrapperIndex` join are already language-neutral — but exploration found three Python-specific blockers:

1. **Module scheme mismatch**: Python's `module_path` is DOTTED (`src.app.client`); `WrapperIndex::lookup` derives caller modules SLASHED and its import-scoped path is relative-`./`-only — a dotted-registered Python wrapper misses both.
2. **LATENT BUG (shipped in F2)**: Python `emit_import` records the FULL statement text (`"from services.api_client import api_get"`) as `RawImport.raw`, so `java/constant_resolver.rs:146`'s dotted-direct constant lookup is dead code — Python cross-file constant resolution via imports never works.
3. **Verb model**: Python wrappers hard-code the verb per function (`requests.get` inside `api_get`), unlike TS (verb from caller options) — `HttpWrapperDef` needs `fixed_method: Option<String>` and a join-side override.

Consumer audit of Python `RawImport.raw` (all readers, verified): every existing read either never matched the full-statement text (dead → live or neutral: `constant_resolver.rs:139/:146/:189`, `emit.rs:494`, `file_cache.rs:252` ImporterIndex — conservative-safe) or is `is_static`-gated (never Python). `import_bindings` has zero readers outside parsers. Recording dotted modules is safe and is the honest fix.

**Other verified anchors**: python `try_emit_http_contract` :799 requires `attribute` callee (`requests|httpx`) — bare-identifier arm slots as a new branch; `requests.request("POST", url)` → method literal arg 0, URL arg 1; `positional_argument` :870 skips kwargs (so `json=data` can't shift the URL); NO first-param reader exists (`parameter_count` unconditionally subtracts 1 — don't reuse); walk hooks = `function_definition` :729 AND `decorated_definition` :670; f-string `{param}` → Dynamic today, concat bare identifier → ungated ConstRef — the wrapper fold must special-case the param name in BOTH positions; `PARSE_CACHE_SCHEMA = 4`, `GOLDEN = (4, "8ff4935ee47dc295")`; headroom has no strong wrapper (verification = synthetic fixture + headroom/eval regression); `HttpWrapperDef` constructor churn for the new field: exactly 2 TS parse sites + 1 resolve-test helper.

Branch `dev`. At implementation start copy this plan to `/Users/phuc/BigMoves/AI/python-wrapper-plan.md` and `docs/plans/python-wrapper-following.md` (committed with Commit 1).

## Commit 1 — `fix(python): record imports as dotted modules; normalize dotted module lookups`

A standalone bugfix commit (prerequisite for every wrapper join path):

- **`python/parse.rs emit_import` (:451)**: `import a.b [as c]` → one RawImport per name, `raw = "a.b"`; `import os, sys` → one each; `from a.b import x, y` → ONE RawImport `raw = "a.b"` (tree-sitter `module_name` field); `from a.b import *` → `is_wildcard = true`; relative `from .x import y` → normalize against `builder.rel`'s dir (one dir stripped per leading dot beyond the first), un-normalizable → record node text as-is (miss, never guess). `import_bindings` (:1082) follows automatically.
- **`cih-resolve/src/contracts.rs WrapperIndex::lookup` (:225)**: after the slashed same-module try, when `caller_pf.language == "python"` also try `stripped.replace('/', ".")` (language-gated so a TS `src/api.ts` can never cross-match a python `src.api`); import-scoped adds a direct `by_key.get(&(imp.raw, callee))` try beside `resolve_relative_module` (dotted python raws ARE keys; TS `./x` raws never are).
- **`java/constant_resolver.rs` branch (a) (:123)**: after the slashed extension-strip try, also try its dotted form (fixes python module-level `File:`-owner sites). Branch (b) :146 becomes live via the raw change — tests only.
- **Schema**: `PARSE_CACHE_SCHEMA` 4→5 + new `GOLDEN` (the corpus python fixture's `import requests` output changes).
- **Tests**: `python_imports_record_dotted_modules` (import/as/from/multi/wildcard/relative forms); `python_constant_resolves_via_from_import` (resolve.rs: decoy same-named constant kills unique fallback; caller imports `services.settings` → still folds — proves the dead branch is live); `python_module_level_site_resolves_same_file_constant` (dotted branch-(a)).

## Commit 2 — `feat(python): follow same-repo HTTP wrappers (detect defs, provisional sites, fixed-method join)`

Merged parser+resolve deliberately: python method correctness depends on the resolve-side `fixed_method` override — a parser-only commit would join `api_post` sites as GET (a wrong-output bisect state). The override is ~6 lines.

### IR (`cih-core/src/ir.rs`)
`HttpWrapperDef` gains `#[serde(default, skip_serializing_if = "Option::is_none")] pub fixed_method: Option<String>` — doc: script wrappers that hard-code the verb; overrides the site's placeholder at join; `None` for TS options-object wrappers. Fix 3 constructors (`typescript/parse.rs:781/:819` → `None`; resolve-test `wrapper_file` helper).

### Detection (`python/parse.rs`, mirroring the TS functions)
- `Builder.http_wrappers` + plumb into the success-path `ParsedFile` (error path stays empty).
- Hooks: `function_definition` arm AND `decorated_definition` function branch, when `class_fqn.is_none() && enclosing.is_none()`, call `try_collect_py_http_wrapper(name, node, src, builder)`.
- `first_py_param_identifier`: first named child of `parameters` that is `identifier`/`typed_parameter`(→ inner identifier); `self`/`cls` → None (do NOT reuse `parameter_count`).
- `find_inner_py_http_call`: recursive body scan skipping `function_definition|decorated_definition|lambda|class_definition`; first `call` with `attribute` func, object `requests|httpx`, attr in `python_http_verb` or `"request"`.
- Method+URL: verb form → `(VERB, arg 0)`; `request` form → literal arg 0 via `literal_py_string` → `(METHOD, arg 1)`; non-literal method → bail (pass-through method wrappers deferred).
- One-level indirection: URL expr an identifier ≠ param → `find_unique_py_assignment(body, local)` over `assignment` nodes (left identifier == local; skip nested defs; count > 1 → bail); `== param` → falls through as pure pass-through (empty prefix).
- `fold_wrapper_py_url_expr`: mirror `fold_py_url_expr` with param checks ORDERED FIRST in both positions — interpolation inner identifier == param → `Param`; bare identifier == param → `Param` (before the ungated ConstRef arm). Local 4-line `WrapperUrlPiece` enum (TS's is module-private).
- Validation identical to TS: param LAST, exactly one, no Dynamic in prefix → `HttpWrapperDef { name, module: builder.module /* dotted */, prefix_parts, options_arg_index: 1, fixed_method: Some(method), range }`.

### Provisional sites (`try_emit_http_contract`)
New `identifier` arm: URL-ish arg 0 gate via new `py_arg_is_url_ish` (fold → first part is `Lit` starting `/`); emit `via_wrapper: Some(callee)`, `http_method: Some("GET")` placeholder, `url_parts` ALWAYS (all-Lit included; empty → return). Module-attribute callees (`api_client.api_get(...)`) out of scope v1 — document. Existing requests/httpx path untouched.

### Resolve override (`contracts.rs` wrapper branch)
After lookup: effective method = `def.fixed_method.as_deref().or(site.http_method.as_deref())`. TS defs carry `None` → unchanged (pin with a test).

### Schema + corpus
`PARSE_CACHE_SCHEMA` 5→6; guard corpus gains `services/api_client.py` (env-default `API_BASE`, `api_get` f-string indirection, `api_post` concat) + a `from services.api_client import api_get` caller; `GOLDEN = (6, <printed>)`.

### Tests
Parser: detection (f-string indirection GET def w/ `ConstRef("API_BASE")` prefix + dotted module; concat POST w/ `json=` kwarg skipped; `request("POST", …)` form; pure pass-through both direct and via `url = path`); rejections (`self` first param in class, nested-def closure, param mid-URL, no inner call, two `url =` assignments, non-literal request method, decorated module-scope def IS detected); provisional (`api_get(f"/admin/items/{item_id}")` shape, all-Lit parts kept; negatives `t("common.x")`, `helper(item_id)`; NOT `log("/msg")` — that's a resolve-drop pin, not a parser negative); regressions (direct requests sites, instance clients).
Resolve: dotted same-module join (same-file caller); from-import scoped join with decoy (proves direct-raw try + two-context fold: `base_source: env_default`, `via_wrapper: "services.api_client#api_get"`); `fixed_method` POST override; pass-through empty prefix (`/health` folds; all-dynamic suffix drops); TS-method-still-from-options no-regression pin; `unmatched_python_provisional_vanishes` (`log("/msg")`).

## Commit 3 — `docs(architecture): document python HTTP wrapper following`
Extend the "Same-repo HTTP wrappers" ARCHITECTURE.md section: python detection rules, dotted-module import recording, the `requests.request` literal form, fixed-verb semantics, v1 limits (module-attribute callees, `import ... as` aliases rescued only by unique-name, method-param pass-throughs bail, session/client instances out of scope). Mark the plan doc completed with live numbers.

## Verification

Per commit: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.
Live after all commits (release build):
1. **Synthetic e2e** in the scratchpad: the Context example repo → analyze → assert `ExternalEndpoint GET /api/v1/admin/items/{*}` and `POST /api/v1/...` with `via_wrapper` + `base_source: env_default` props and ExternalCall edges from caller functions.
2. **Headroom regression**: re-analyze; expect byte-identical endpoints (its lone pass-through wrapper's call sites use `http://localhost...` — fails the leading-`/` gate). Any additive delta must map to a real `from X import CONST` fold (the Commit-1 payoff) — inspect and record each.
3. **Java eval byte-identity**: servicemix/fineract routes + edge reasons unchanged.
4. **TS regression**: re-analyze 212ecom-fe → still exactly 163 ExternalEndpoints.

## Risks — resolved in design

| Risk | Resolution |
|---|---|
| Dotted try cross-matches TS caller ↔ python wrapper | Gated on `caller_pf.language == "python"` |
| Import-raw change perturbs Imports edges / ImporterIndex | Full reader audit: every read was dead-or-neutral for full-statement raws; ImporterIndex expansion only widens (safe); headroom diff is the empirical check |
| Provisional false positives (`redirect("/home")`) | Dropped at resolve unless a repo-unique DETECTED wrapper shares the name; pinned by drop test (same accepted risk as TS `navigate`) |
| `json=data` kwarg shifting the URL arg | `positional_argument` skips keyword_arguments — pinned by the concat-POST test |
| Wrapper param folded as ConstRef by the ungated concat arm | Param checks ordered BEFORE the ConstRef arm in the wrapper fold |
| `url` reassigned in body | Unique-assignment finder bails on count > 1 |
| Two schema bumps in one push | Deliberate — each output-changing commit pairs (schema, GOLDEN) self-consistently for bisect |
| `import x as y` aliases | by_key misses (name ≠ alias) → unique_by_name rescues iff repo-unique; documented v1 limit (parity with TS) |
