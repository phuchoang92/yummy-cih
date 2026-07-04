# Plan: persistent settings (`cih.toml`) for cih-engine commands

> On approval, copy this to `docs/plans/config-settings.md` in the repo (project-local plan preference), then implement.

## Context

`cih-engine analyze` (~12 flags), `discover` (~15), and `wiki` (~25) require re-typing
the same non-default options every run — LLM provider/model, community/feature strategy,
trace-depth params, wiki mode/grouping, etc. There's no persisted default and no way to
see what settings are in effect. The repo already has the right *pattern* (per-repo TOML:
`cih.scope.toml` via `scope.rs`, `cih.taint.toml` via `cih_taint::load_taint_rules`,
`cih.decompile.toml` with an interactive `cih config decompile` editor in
`crates/cih-engine/src/cmd/config.rs`) — it just doesn't cover command-option flags.

Goal: a unified, layered settings file that supplies defaults for `analyze`/`discover`/
`wiki`, plus `cih config show` (effective values annotated by source) and `cih config init`
(scaffold the file). Decisions confirmed with the user:

- **One unified `cih.toml`** with `[analyze]` / `[discover]` / `[wiki]` sections. Existing
  `cih.scope.toml` / `cih.taint.toml` / `cih.decompile.toml` stay as-is.
- **Layered precedence**: `CLI flag > env var > <repo>/cih.toml > ~/.cih/config.toml > built-in default`.
- **First pass = settings loading + `config show` + `config init`.** TUI wiring deferred.

## Design

### 1. New module `crates/cih-engine/src/settings.rs`

- `CihSettings { analyze: AnalyzeSettings, discover: DiscoverSettings, wiki: WikiSettings }`,
  all leaf fields `Option<T>` (absent = "not set at this layer"). `#[derive(Deserialize, Default)]`,
  `#[serde(default)]` so partial files are valid — mirror `ScopeRequest`/`TomlRules`.
- Loader `CihSettings::load(repo: &Path) -> CihSettings`: read `~/.cih/config.toml` (home,
  via `cih_core`'s home resolution — same base as `Registry`/`contracts_path`), then
  `<repo>/cih.toml`; **merge** with repo overriding home (per-field `Option::or`). Missing
  files are not errors (return defaults); a malformed file logs a `tracing::warn!` and is
  skipped — same fail-soft behavior as `load_taint_rules`.
- A `Resolved<T> { value: T, source: Source }` where `Source ∈ {Default, HomeConfig,
  RepoConfig, Env, Flag}`, and a helper `resolve(flag: Option<T>, env: Option<T>,
  repo: Option<T>, home: Option<T>, default: T) -> Resolved<T>` applying precedence and
  recording the winning source. Powers both real resolution and `config show`.

### 2. Make layerable flags detectable in `crates/cih-engine/src/main.rs`

The blocker: fields with clap `default_value = "…"` can't distinguish an explicit value
from the clap-filled default. Convert the **durable-preference** options on `Analyze`,
`Discover`, `Wiki` from `default_value` to `Option<T>` (drop the attribute); move each
built-in default into a `const` in `settings.rs` used by the resolver. Fields already
`Option<T>` (e.g. `resolution`, `min_community_size`, `max_trace_depth`) need no change.

Config-backed vs per-run (stays flag-only, never in `cih.toml`):
- **analyze**: `languages`, `skip_xml_integration`, `include_decompiled` → config; `repo`,
  `all/module/include/exclude`, `scope`, `json`, `no_cache` → per-run.
- **discover**: `community_strategy`, `resolution`, `min_community_size`, `max_trace_depth`,
  `max_processes`, `max_branching`, `min_trace_confidence`, `feature_strategy`,
  `feature_llm_provider/model/base_url/api_key_env/max_tokens/timeout_secs` → config;
  `json` → per-run.
- **wiki**: `llm`, `llm_provider/base_url/model/api_key_env/max_tokens/timeout_secs/retries/
  concurrency`, `wiki_language`, `wiki_mode`, `grouping`, `html`, `incremental` → config;
  `repo`, `out`, `evidence`, `filter_*`, `max_communities`, `save_evidence`,
  `llm_debug_evidence`, `llm_dry_run`, `json` → per-run.

Note the LLM params appear in both `discover` (`feature_llm_*`) and `wiki` (`llm_*`) with
different names — keep them per-section in v1 (document the overlap; a shared `[llm]`
section is a possible follow-up).

### 3. Resolve at the dispatch site

In the `Command::Analyze`/`Discover`/`Wiki` match arms (main.rs ~525/569/758), load
`CihSettings::load(&repo)` once, then resolve each option through the precedence helper
before constructing the existing concrete `AnalyzeFlags` / discover args / `wiki::WikiConfig`.
The `run_analyze` / `run_discover` / `run_wiki` signatures **do not change** — they still
receive fully-resolved concrete values. Env layer applies where an env binding exists
(e.g. `FALKOR_URL`, `CIH_GRAPH_KEY` already on `DbArgs`); most option flags have no env and
resolve `flag > repo > home > default`.

### 4. New `ConfigCommand` variants (main.rs enum ~417, impl in `cmd/config.rs`)

- `cih config show [--repo <path>] [--json]`: load settings, compute effective value + source
  for every config-backed option, print grouped by `[analyze]/[discover]/[wiki]` with a
  trailing `(default|~/.cih/config.toml|cih.toml|env)` tag per line (reuse the `Resolved`
  source). `--json` for machine use.
- `cih config init [--repo <path>] [--global]`: write a starter `cih.toml` (repo) or
  `~/.cih/config.toml` (`--global`) pre-populated with the current effective defaults,
  each line commented with its built-in default; refuse to clobber an existing file unless
  `--force`. Follow the print/prompt style of `run_config_decompile`.

## Critical files

- `crates/cih-engine/src/settings.rs` — **new**: `CihSettings`, loader, `Resolved`/`Source`, resolver, default consts.
- `crates/cih-engine/src/main.rs` — flag `Option<T>` conversions; load+resolve in the three dispatch arms; new `ConfigCommand::Show`/`Init` variants; `mod settings;`.
- `crates/cih-engine/src/cmd/config.rs` — add `run_config_show` / `run_config_init` beside `run_config_decompile`.
- `crates/cih-engine/src/lib.rs` — export `settings` if referenced by tests.
- Reuse: `ScopeRequest::from_toml` (scope.rs) and `cih_taint::config` as the TOML-load template; `cih_core` home-dir resolution (as `Registry`/`contracts_path` use); `run_config_decompile` as the editor/writer template.

## Docs

- README: new "Configuration" section — `cih.toml` sections, precedence ladder, `config show`/`init`.
- `docs/agent-workflows/` unaffected; note `cih.toml` in CLAUDE.md's "Developing CIH" block.

## Verification

- Unit tests in `settings.rs`: merge (repo overrides home; partial files), and `resolve`
  precedence + `Source` for each layer (flag/env/repo/home/default).
- `cargo test -p cih-engine`; `cargo build -p cih-engine`.
- Manual: `cih config init` writes `cih.toml`; edit `[discover] feature_strategy="hybrid"`;
  `cih config show` reports it as `(cih.toml)` while unset options show `(default)`; run
  `cih-engine discover <repo>` and confirm it uses the file value without the flag, and that
  passing `--feature-strategy package` still overrides (source `flag`). Repeat one wiki option
  to confirm the same layering, and `--global` writing `~/.cih/config.toml`.
- Regression: existing commands with no `cih.toml` behave exactly as today (defaults unchanged).

## Out of scope (follow-ups)

TUI pre-fill + "save as defaults" (`tui.rs`), folding scope/taint/decompile into `cih.toml`,
a shared `[llm]` section deduplicating discover/wiki LLM params.
