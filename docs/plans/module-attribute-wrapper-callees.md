# Module-attribute wrapper callees: `api.api_get("/x")` / `import * as api` join wrapper defs

## Context

The wrapper-following stack (TS `01141ec..46b44a4`, Python `ecf2cea..7e7feb6`) covers bare-identifier callees (`from services.api_client import api_get; api_get(...)` / named TS imports). The other common import style is module-attribute calls: Python `import services.api_client as api; api.api_get("/x")` (plus the full dotted receiver `services.api_client.api_get(...)` — note: `import a.b; b.f()` is INVALID Python, only `a` gets bound, so "non-aliased" coverage means the dotted-receiver form) and TS namespace imports `import * as api from './apiClient'; api.apiFetch('/x')`. Today both are documented v1 limits: import ALIASES are dropped at parse (`RawImport` has no alias field) and member/attribute callees never emit provisional sites.

**Design (from verified exploration + design review):**
- `RawImport` gains `alias: Option<String>` (serde-neutral for None; **17** literal constructors break mechanically — 13 parsers + 4 test sites incl. both `import()` helpers).
- Parse-side provisional emission for member/attribute callees is **gated on known import bindings in the same file** (`builder.imports` is available; single pre-order DFS means imports lexically before the call — function-local late imports missed, documented). This keeps `instance_clients_are_not_emitted` green WITHOUT relaxing it: `myobj`/`notaxios` match no import; `this.http` is a member-expression receiver, naturally excluded.
- `via_wrapper` carries `"obj.attr"`; resolve splits with `rsplit_once('.')` (dotted receivers keep their dots in obj) and resolves the module via the caller's imports ONLY — **no unique-name fallback for dotted callees** (the receiver pins the module; miss → drop, never guess).
- OUT OF SCOPE (document): TS default imports, tsconfig path aliases (`@/…` never relative-resolves → silent drop), Python `from x import y as z` name aliases, function-local imports after the call site. Do NOT wire the new alias into the `ImportBinding` conversions (both hardcode `local: None`) — that would perturb `cih-resolve/src/lang/*` type-binding resolution; note as follow-up.

**Verified anchors**: RawImport at `cih-core/src/ir.rs:277-286`; python `aliased_import` exposes field `alias` (identifier) — current code (`python/parse.rs:767-771`) reads only `name`; TS namespace alias = the identifier named child of `import_clause > namespace_import` (no field name — iterate); TS member arm (axios-only) at `typescript/parse.rs:512-524`; python attribute arm at `python/parse.rs:1192-1200`; `WrapperIndex::lookup` at `cih-resolve/src/contracts.rs:232-272`; `PARSE_CACHE_SCHEMA = 6`, `GOLDEN = (6, "7e1d2f89613326e6")`; go/java/kotlin emit `via_wrapper: None` — untouched by the split.

Branch `dev`. At implementation start copy this plan to `/Users/phuc/BigMoves/AI/module-attr-callee-plan.md` and `docs/plans/module-attribute-wrapper-callees.md` (committed with Commit 1).

## Commit 1 — `feat(ir): record import aliases` (alias captured, unconsumed)

- **`cih-core/src/ir.rs`** `RawImport`: `#[serde(default, skip_serializing_if = "Option::is_none")] pub alias: Option<String>` — doc: python `import a.b as c` / TS `import * as c from './m'` → `Some("c")`.
- **17 constructors** get `alias: None`: parsers cpp:39, csharp:53, elixir:81, go:195+209, java/parse/mod.rs:680, kotlin:303, php:55, python:800, ruby:45, rust:212, scala:53, typescript:409; tests core.rs:204, file_cache.rs:68, cih-resolve/src/tests.rs:99, cih-resolve/tests/resolve.rs:106. Extend both `import()` helpers; add `aliased_import(raw, alias)` helper to resolve.rs for Commit 2.
- **Python capture** (`emit_import`): `raws` becomes `Vec<(String, bool, Option<String>)>`; the `aliased_import` arm also reads `child_by_field_name("alias")`. From-import name aliases stay uncaptured.
- **TS capture** (`emit_import`): while iterating children, on `import_clause` iterate ITS children for `namespace_import` → alias = its first named `identifier` child. Default/named imports stay `alias: None`.
- **Schema 6→7 + corpus**: add `import services.api_client as api` to a python fixture and `import * as api from './apiClient';` to a TS fixture (pins the capture; forces the hash change) → `GOLDEN = (7, <printed>)`.
- **Tests**: `python_imports_record_aliases` (`import a.b as c` → alias Some("c"); plain import → None; `from a.b import x as y` → raw a.b, alias None); TS `namespace_import_records_alias` (+ default/named → None); existing dotted-modules test stays green.

## Commit 2 — `feat(contracts): module-attribute wrapper callees`

### Python parse arm (`python/parse.rs try_emit_http_contract`, attribute gate :1192-1200)
Restructure: `requests|httpx` identifier objects keep the existing direct path FIRST; otherwise the module-attribute candidate path:
- Gate: `py_import_binds_module(&builder.imports, obj.kind(), &obj_text)` — true when any non-static import has (a) `alias == obj` (aliased import), (b) `obj_kind == "attribute" && alias.is_none() && raw == obj_text` (full dotted receiver `services.api_client.api_get`), or (c) `obj_kind == "identifier" && alias.is_none() && raw.rsplit('.').next() == Some(obj)` (last-segment belt-and-braces — can only over-emit provisionals that drop).
- Then the standard provisional emission: URL-ish arg 0 (`py_arg_is_url_ish`), parts always, placeholder GET, `via_wrapper = Some(format!("{obj_text}.{attr}"))`.
- `import requests as r; r.get(url)` → provisional `r.get` that drops at resolve (no wrapper def under ("requests","get")) — accepted noise, noted.

### TS parse arm (`typescript/parse.rs` member arm :512-524)
Keep the axios path first; else: receiver must be a bare `identifier` matching a known import **alias** (namespace imports only); gate `ts_arg_is_url_ish`; `via_wrapper = Some(format!("{obj}.{property}"))`; method via `call_options_method` (default GET). The existing wrapper-parts branch applies unchanged. `instance_clients_are_not_emitted` stays green structurally — do not touch it.

### Resolve lookup split (`cih-resolve/src/contracts.rs`)
`lookup`: if `callee.rsplit_once('.')` → `lookup_module_attr(obj, attr, caller_pf)`; bare-name path byte-identical. New fn: for each non-static import where (alias==obj) or (python: `raw == obj` full-dotted) or (python: last-segment match, alias None): try `by_key[(resolve_relative_module(file, raw), attr)]` (TS) then `by_key[(raw, attr)]` (python dotted). No same-module steps, NO unique-name fallback — miss → None → site drops. Everything downstream (two-context fold, `fixed_method` override, provenance props) unchanged.

### Schema + docs
Schema 7→8; corpus gains the call sites (python `api.api_get(f"/items/{item_id}")`, TS `api.apiFetch('/admin/x', { method: 'POST' })`) → `GOLDEN = (8, <printed>)`. ARCHITECTURE.md: move module-attribute callees out of v1 limits; document the three receiver rules + import-scoped-only resolution; keep the remaining limits list (TS default imports, path aliases, from-import name aliases, late local imports).

### Tests
Parser py: module-attribute provisional (aliased + full-dotted receiver shapes), no-import → empty, non-URL arg → empty, requests/httpx precedence (direct sites keep `via_wrapper: None`).
Parser TS: namespace-alias provisional (POST from options), named-import member call → empty, instance-clients regression untouched.
Resolve: `ts_namespace_alias_call_joins` (decoy kills fallback; via_wrapper provenance asserted); `py_module_alias_call_joins_with_fixed_method` (decoy + env-default two-context fold + POST override); `py_last_segment_module_call_joins` + full-dotted form; `dotted_callee_without_matching_import_drops` (wrapper name repo-UNIQUE yet dotted callee with no matching import → NO endpoint — proves no unique fallback); `aliased_requests_dotted_callee_drops`.

## Verification

Per commit: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.
Live after both commits (release build):
1. Synthetic fixtures in the scratchpad: python repo with aliased (`import services.api_client as api; api.api_get(f"/admin/items/{i}")`) AND full-dotted (`services.api_client.api_get("/items")`) callers over the env-default wrapper → endpoints with `via_wrapper` + `base_source: env_default`; TS repo with a namespace-import caller → `POST /api/…`.
2. Regressions: 212ecom-fe re-analyze → exactly **163** endpoints; headroom → **11** direct endpoints, no false joins; `scripts/eval-enterprise-java.sh` PASS.
3. Mark both plan copies completed with live numbers; push.

## Risks — resolved in design

| Risk | Resolution |
|---|---|
| Invalid-Python "non-aliased" pattern in the original brief | Corrected: alias rule + full-dotted-receiver rule cover real code; last-segment kept as harmless belt-and-braces |
| Instance-client regression (`myobj.get('/x')`) | Parse gate on known import bindings — the test's fixtures import nothing; `this.http` excluded structurally; test NOT relaxed |
| Provisional noise (aliased requests, url-ish args on non-wrapper modules) | Drops at resolve by construction (pinned module, no fallback); optional requests/httpx exclusion if cache size ever matters |
| Shadowing (`api = X()` after the import) | Scope-blind parse, but a wrong join requires a wrapper def at exactly the pinned module+name — improbable; "degrade, never guess" holds |
| tsconfig path aliases / barrels | `resolve_relative_module` → None → silent drop; documented limit |
| ImportBinding conversions perturbing lang resolvers | Deliberately NOT wired (both hardcode `local: None`); follow-up noted |
| Double GOLDEN churn (7 then 8) | Guard enforces paired updates per output-changing commit — bisect-correct by design |
