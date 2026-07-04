# Phase A — Hardening before team / banking rollout

## Context

`taint_paths` landed, so the MCP surface is feature-complete enough to point an SDLC
agent at. Before a shared team server indexes the banking codebase, four gaps need
closing: there is no CI gating the ~780 tests, the HTTP server runs unauthenticated by
default, `read_file` can pull an unbounded file into an agent's context, and the repo's
`CLAUDE.md` documents a different tool (GitNexus) than what this repo is. None are large;
together they're the difference between "works on my machine" and "safe for the team."

This plan is independent of the SDLC-agent build (Phase B) — none of it blocks that work,
and it can land in parallel.

## Scope (4 work items + 1 doc fix)

### A1 — CI pipeline

**Problem:** No `.github/workflows/`. ~780 tests exist with nothing gating them; a broken
`main` is invisible until someone builds locally.

**Do:**
- Add `.github/workflows/ci.yml`, triggered on push + pull_request.
- One job, `stable` toolchain via `rust-toolchain.toml` (already pins `stable` + rustfmt +
  clippy), with `Swatinem/rust-cache` for target caching.
- Steps: `cargo fmt --all --check` → `cargo clippy --workspace --all-targets -- -D warnings`
  → `cargo test --workspace`.
- The taint/embed integration paths need FalkorDB + Postgres only for a subset of tests.
  Keep CI hermetic: run the default `cargo test --workspace` (unit + artifact-fixture tests
  like the new `taint::tests` run with no services). If any test needs FalkorDB, gate it
  behind a feature/`#[ignore]` and add a separate opt-in job with a `falkordb/falkordb` +
  `pgvector/pgvector` services block — do **not** make the core job depend on services.

**First run will likely fail clippy** — the crates have many `dead_code`/`unused_imports`
warnings today (visible in editor diagnostics). Two options, pick per appetite:
- Fast: scope `-D warnings` to the crates on the agent's critical path
  (`cih-server`, `cih-taint`, `cih-falkor`, `cih-graph-store`, `cih-core`) and leave the
  rest at default-warn for now.
- Thorough: a cleanup pass first (`cargo fix` + manual) then `-D warnings` workspace-wide.

Recommend **fast** now, thorough as a follow-up — don't let a warning backlog block the gate.

**Files:** `.github/workflows/ci.yml` (new).
**Verify:** push a branch, confirm the check runs and goes green; break a test locally to
confirm it would fail.

### A2 — Mandatory auth on non-localhost bind

**Problem:** `crates/cih-server/src/main.rs:644` only *warns* when `CIH_API_TOKEN` is unset,
and `docker-compose.yml` binds `0.0.0.0:8080`. A team server is therefore open by default.
Auth middleware already exists (`server::auth_middleware`, wired at `main.rs:671`) and the
token is read in `config.rs:73` — the gap is purely that nothing enforces it.

**Do:** in `main()` after `Config::from_env()`, if `api_token.is_none()` **and** the bind
host is not loopback, return an error and exit instead of warning. Parse the host from
`cfg.bind`; treat `127.0.0.1`, `::1`, and `localhost` as loopback. Keep the existing warn
for the loopback-dev case. Add an escape hatch env `CIH_ALLOW_INSECURE=1` for users who
deliberately run open on a trusted network, so we fail safe without being unbypassable.

**Files:** `crates/cih-server/src/main.rs` (the `if cfg.api_token.is_none()` block),
`crates/cih-server/src/config.rs` (read `CIH_ALLOW_INSECURE`). Add a unit test for the
loopback-detection helper.
**Verify:** `CIH_BIND=0.0.0.0:8080 cih-server` (no token) exits with a clear message;
adding `CIH_API_TOKEN=x` or `CIH_ALLOW_INSECURE=1` lets it start; `127.0.0.1` bind still
starts with only a warning.

Also add **SECURITY.md** at repo root: document the `ask_codebase` data-egress (sends
symbol names, file paths, and search snippets to the configured LLM — default Gemini —
when `CIH_AGENT_API_KEY` is set; off by default), the `CIH_API_TOKEN` requirement, and the
recommendation to leave `ask_codebase` disabled for the banking deployment and do LLM
reasoning in the downstream agent.

### A3 — `read_file` size cap

**Problem:** `crates/cih-server/src/files.rs:20` does `read_to_string` on the whole file,
then slices. A large or binary file goes fully into memory and, without `start/end_line`,
fully into the agent's context window.

**Do:**
- Add a byte cap (default 10 MB) checked via `std::fs::metadata` before reading; over it,
  return an `invalid_params` error telling the agent to pass `start_line`/`end_line`.
- Add a returned-lines cap (e.g. 5000) when no explicit range is given; when the slice is
  truncated, set a `"truncated": true` field and a note in the JSON so the agent knows to
  narrow the range rather than assuming it saw the whole file.
- Make both configurable via env (`CIH_READ_FILE_MAX_BYTES`, `CIH_READ_FILE_MAX_LINES`),
  read in `config.rs`, passed through to `files::read_file`.

**Files:** `crates/cih-server/src/files.rs`, `crates/cih-server/src/config.rs`,
`crates/cih-server/src/main.rs` (thread the limits into the `read_file` tool call).
**Verify:** unit test with a temp file over the byte cap (errors) and one over the line cap
(returns `truncated: true`); a normal small file is unchanged.

### A4 — Heuristic edge-case tests (correctness backbone)

**Reframed from the original review.** `cih-lang` and `cih-parse` are *not* untested —
`crates/cih-lang/tests/java.rs` (6 tests: Spring MVC routes, JAX-RS, stereotypes,
scan_file) and `crates/cih-parse/tests/sql.rs` + `sql_detection.rs` (19 tests) cover the
happy paths. The real gap is **edge cases that produce silently-wrong graph edges**, which
is the one failure mode an impact/taint answer cannot tolerate.

**Do — add golden/edge-case cases to the existing test files** (no new harness needed):
- Routes (`framework.rs` via `tests/java.rs`): class-level `@RequestMapping` prefix + method
  `@GetMapping` path concatenation (leading/trailing slash normalization); `@RequestMapping`
  with explicit `method = RequestMethod.POST`; multiple paths in one annotation; missing
  path (defaults to class prefix). These feed `route_map` and taint sources directly.
- SQL (`sql.rs`): table extraction from JOINs, subqueries, `INSERT INTO`/`UPDATE`/`DELETE`,
  and the dynamic-SQL case that must yield `dynamic=true` with no table node
  (`cih-resolve/src/db_access.rs:4` limitation — assert the documented behavior).
- Add an ARCHITECTURE.md "parser assumptions" section (or a doc-comment block) enumerating
  the known limits: same-file/same-class DB-constant resolution, no Feign URL interpolation,
  dynamic SQL not table-resolved — so agent answers can carry the caveat.

**Files:** `crates/cih-lang/tests/java.rs`, `crates/cih-parse/tests/sql.rs`,
`docs/ARCHITECTURE.md` (new) or module doc-comments.
**Verify:** `cargo test -p cih-lang -p cih-parse` green with the new cases.

### A5 — Fix CLAUDE.md

**Problem:** repo `CLAUDE.md` documents **GitNexus** (a different tool) — wrong tool names
(`explain`, `rename`), wrong MCP resource URIs (`gitnexus://...`), wrong CLI. Confusing for
any human or agent onboarding to CIH.

**Do:** replace with CIH-specific guidance: the real tool names (from `main.rs`
`get_info()`), the `cih://repo/{name}/...` resource URIs (`resources.rs`), and a pointer to
`docs/agent-workflows/` (including the new `security.md`). Keep the "run impact before
editing / detect_changes before committing" workflow ethos — that part is good, just retarget
it to CIH tool names.

**Files:** `CLAUDE.md`.

## Suggested order

A1 (CI) first so everything after is gated. Then A2 + A3 (server hardening, same crate, one
build). A4 (tests) and A5 (docs) can go anytime, ideally before the banking index so the
caveats are written down. A2's SECURITY.md and A5's CLAUDE.md are pure docs — cheap, do them
alongside their code items.

## Global verification

- `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` clean locally and in CI.
- Manual auth check: server refuses `0.0.0.0` bind without token; accepts with token or `CIH_ALLOW_INSECURE`.
- Manual `read_file` check: oversized file errors, over-long slice reports `truncated`.
- No behavior change to any existing MCP tool response (only `read_file` gains `truncated`).

## Explicitly out of scope (Phase B / C)

Building the SDLC agent, multi-repo routing, DbTable reverse-impact query, `@Transactional`
/ PII analyses, taint-result caching. Tracked in `review-the-yummy-cih-repo` plan.
