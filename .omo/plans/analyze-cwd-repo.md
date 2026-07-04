# analyze-cwd-repo - Work Plan

## TL;DR (For humans)

**What you'll get:** `cih-engine analyze` will work from inside the repository without typing the repo path, while the existing explicit-path form still works exactly as before.

**Why this approach:** The safest change is to default only the missing repo path to the current folder, then reuse the analyzer's existing validation and indexing pipeline. This avoids changing analyzer internals or surprising users with automatic full-repo indexing.

**What it will NOT do:** It will not make `analyze` imply “index everything.” It will not change the repo argument behavior for other commands. It will not restructure the analyzer pipeline.

**Effort:** Short
**Risk:** Low - one public CLI parse change plus TUI metadata, protected by targeted parser/runtime tests.
**Decisions to sanity-check:** Missing repo path means current directory; missing scope still behaves as today and does not imply `--all`.

Your next move: choose whether to start implementation now or run a high-accuracy plan review first. Full execution detail follows below.

---

> TL;DR (machine): Short/low-risk CLI change: make only `cih-engine analyze` repo positional optional, resolve omitted repo to cwd, keep explicit repo and scope behavior, update TUI metadata, add targeted Rust tests and evidence.

## Scope

### Must have

- `cih-engine analyze --all` run from a repo directory resolves the repo path to `std::env::current_dir()` and starts the existing analyzer path.
- `cih-engine analyze /explicit/repo --all` keeps parsing and dispatching an explicit repo path.
- `cih-engine analyze` without a scope selector keeps existing behavior: it scans/writes the repo map, prints the existing “Choose a scope: --all | --module <names> | --include <glob> | a cih.scope.toml” guidance, and exits via the existing `process::exit(2)` path when no `cih.scope.toml` supplies a selector.
- `cih-engine analyze` with an existing `cih.scope.toml` continues to use that file through the existing `build_scope_request` logic.
- The interactive TUI command builder no longer treats analyze's repo field as required; leaving it blank omits the positional repo argument so the CLI uses cwd.
- Explicit repo entry in the TUI still emits the repo path before flags.
- The error for failing to determine cwd includes actionable context telling the user to pass an explicit repo path or run from a valid directory.
- Add tests that prove omitted repo parsing, explicit repo parsing, omitted repo dispatch/runtime behavior, explicit repo dispatch/runtime behavior, and TUI blank/filled repo command assembly.

### Must NOT have (guardrails, anti-slop, scope boundaries)

- Do not make `cih-engine analyze` imply `--all`.
- Do not change repo argument behavior for `scan`, `resolve`, `discover`, `embed`, `wiki`, `taint`, or `artifact` subcommands.
- Do not change `analyze::run_analyze(repo: PathBuf, flags: AnalyzeFlags)` or `scan::scan_repo(repo: &Path)` signatures unless a compiler error proves there is no smaller fix.
- Do not move analyzer logic out of `analyze::run_analyze`; resolve cwd in the CLI dispatch layer before the call.
- Do not add `assert_cmd` or other new test dependencies unless parser/unit tests cannot cover the required behavior.
- Do not update README or broad docs unless directly required by compiler/test failures; help text/TUI metadata are in scope.
- Do not touch unrelated dirty worktree files, especially `.omo/run-continuation/*`.

## Verification strategy

> Zero human intervention - all verification is agent-executed.

- Test decision: tests-after using Rust unit/integration tests already present in `cih-engine`.
- Primary commands:
  - `cargo fmt --all -- --check`
  - `cargo test -p cih-engine`
  - `cargo clippy -p cih-engine -- -D warnings`
- Mandatory pre-edit safety: before editing each product symbol, rerun impact analysis for that symbol and record risk in `.omo/evidence/task-1-analyze-cwd-repo.md`:
  - `main` in `crates/cih-engine/src/main.rs`
  - `run_analyze` in `crates/cih-engine/src/analyze/mod.rs` only if touched; expected plan avoids touching it
  - `make_commands`, `to_shell_fragment`, or `assembled_command` in `crates/cih-engine/src/tui.rs` if touched
- Evidence files:
  - `.omo/evidence/task-1-analyze-cwd-repo.md` — impact analysis and implementation diff summary
  - `.omo/evidence/task-2-analyze-cwd-repo.md` — parser/runtime test additions and outputs
  - `.omo/evidence/task-3-analyze-cwd-repo.md` — TUI metadata/test additions and outputs
  - `.omo/evidence/task-4-analyze-cwd-repo.md` — final fmt/test/clippy outputs

## Execution strategy

### Parallel execution waves

- Wave 1 is sequential because CLI shape and test targets depend on each other.
- Wave 2 test/TUI work can be partially parallelized only after the parser shape compiles.
- Final verification runs after all code and tests are complete.

### Dependency matrix

| Todo | Depends on | Blocks | Can parallelize with |
| --- | --- | --- | --- |
| 1 | none | 2, 3, 4 | none |
| 2 | 1 | 4 | 3 after CLI compiles |
| 3 | 1 | 4 | 2 after CLI compiles |
| 4 | 2, 3 | final verification | none |

## Todos

> Implementation + Test = ONE todo. Never separate.
<!-- APPEND TASK BATCHES BELOW THIS LINE WITH edit/apply_patch - never rewrite the headers above. -->

- [x] 1. `crates/cih-engine/src/main.rs`: Make analyze repo optional and resolve cwd before dispatch - expect explicit and omitted repo forms to compile
  What to do / Must NOT do: Run impact analysis for `main` in `crates/cih-engine/src/main.rs` before editing and record the result. Change only the `Analyze` variant's `repo` field from `PathBuf` to `Option<PathBuf>`. In the `Command::Analyze` match arm, resolve the repo before calling `analyze::run_analyze`: use the explicit `PathBuf` when present; otherwise call `std::env::current_dir()` with `anyhow::Context` explaining that cwd could not be determined and the user can pass an explicit repo path. Keep `AnalyzeFlags` unchanged. Do not change scope-selection logic or add `--all` defaults. Do not change other command variants.
  Parallelization: Wave 1 | Blocked by: none | Blocks: 2, 3, 4
  References (executor has NO interview context - be exhaustive): `crates/cih-engine/src/main.rs:25-29` imports `PathBuf`, `anyhow::Result`, and clap; `crates/cih-engine/src/main.rs:67-103` defines `Command::Analyze`; `crates/cih-engine/src/main.rs:521-553` dispatches analyze; `crates/cih-engine/src/analyze/mod.rs:42-120` shows `run_analyze(repo: PathBuf, flags)` should continue to receive a resolved path; `crates/cih-engine/src/scan.rs:128-132` remains the repo canonicalization/validation point; clap docs confirm `Option<T>` makes a positional optional.
  Acceptance criteria (agent-executable): `cargo check -p cih-engine` succeeds; a code review of the diff shows only `Analyze` repo is optional and no `--all` default was added; evidence includes the exact impact-analysis risk and direct caller count.
  QA scenarios (name the exact tool + invocation): Happy: `cargo check -p cih-engine 2>&1 | tee .omo/evidence/task-1-analyze-cwd-repo.md` exits 0. Failure: inspect `cargo check`/compiler output and fix any type errors without broadening scope; evidence remains `.omo/evidence/task-1-analyze-cwd-repo.md`.
  Commit: Y | `feat(cli): default analyze repo to cwd`

- [x] 2. `crates/cih-engine/src/main.rs` tests: Lock parser/runtime behavior for explicit and omitted analyze repo - expect no scope auto-default
  What to do / Must NOT do: Add the narrowest tests available for the private clap parser. Preferred: add `#[cfg(test)] mod tests` in `main.rs` that imports `clap::Parser` and calls `Cli::try_parse_from`. If binary-private testing is awkward, extract only the CLI type definitions to a small module and re-export internally, but do not refactor analyzer logic. Required parser cases: `['cih-engine', 'analyze', '/tmp/repo', '--all']` yields `repo == Some('/tmp/repo')`; `['cih-engine', 'analyze', '--all']` yields `repo == None`; `['cih-engine', 'analyze']` parses with `repo == None`, proving parse no longer rejects before the existing runtime scope gate. Add a runtime/unit-level assertion where feasible that resolving omitted repo uses `current_dir()` before calling the existing pipeline; avoid actually loading FalkorDB by using `--no-load` and/or existing library-level patterns when a full run is needed. Do not add `assert_cmd` unless parser/unit testing cannot satisfy these criteria.
  Parallelization: Wave 2 | Blocked by: 1 | Blocks: 4
  References (executor has NO interview context - be exhaustive): `crates/cih-engine/src/main.rs:49-57` defines `Cli`; `crates/cih-engine/src/main.rs:67-103` defines analyze parser fields; `crates/cih-engine/src/main.rs:521-553` resolves dispatch; `crates/cih-engine/src/analyze/mod.rs:57-64` preserves no-selector exit behavior; `crates/cih-engine/src/scope.rs:42-44` defines selectors; `crates/cih-engine/tests/crate_tests.rs` has existing temp repo/analyze patterns if integration coverage is needed; `crates/cih-engine/Cargo.toml:49-50` discourages new test dependencies.
  Acceptance criteria (agent-executable): Targeted tests pass and prove all three parser cases. If a runtime no-scope case is added, it must assert the existing selector prompt/exit behavior, not success. Test names must include words like `analyze_omitted_repo`, `analyze_explicit_repo`, and `analyze_no_scope` so they can be run by name.
  QA scenarios (name the exact tool + invocation): Happy: `cargo test -p cih-engine analyze_ -- --nocapture 2>&1 | tee .omo/evidence/task-2-analyze-cwd-repo.md` exits 0 and output includes the new analyze CLI tests. Failure: intentionally review the diff/test assertions to ensure no assertion expects `--all` when omitted; if the test runner cannot filter by `analyze_`, run `cargo test -p cih-engine -- --nocapture` and record why in evidence.
  Commit: Y | `test(cli): cover analyze cwd repo default`

- [x] 3. `crates/cih-engine/src/tui.rs`: Make TUI analyze repo optional and test command assembly - expect blank repo omitted, filled repo retained
  What to do / Must NOT do: Run impact analysis for `make_commands` and `assembled_command`/`to_shell_fragment` before editing if those symbols are touched; record risk. In `make_commands`, update only the `analyze` command's positional repo `Field::text` from required to optional and adjust description/placeholder to explain blank means current directory. Do not change `scan`, `discover`, `embed`, or `wiki` repo fields. Add or update TUI tests so an empty analyze repo field assembles `cih-engine analyze ...` without `<repo>` and without an empty positional argument, while a filled repo assembles with the explicit repo before flags.
  Parallelization: Wave 2 | Blocked by: 1 | Blocks: 4
  References (executor has NO interview context - be exhaustive): `crates/cih-engine/src/tui.rs:135-145` shows optional blank text fields return `None` and required blank fields emit `<label>`; `crates/cih-engine/src/tui.rs:193-207` defines analyze TUI fields and currently marks repo required; `crates/cih-engine/src/tui.rs:321-336` assembles positional args before flags; `crates/cih-engine/tests/start.rs:137-153` shows existing command-plan test style, but TUI internals may need `#[cfg(test)]` in `tui.rs` because helper types are private.
  Acceptance criteria (agent-executable): Tests prove the blank analyze repo field omits `<repo>` and explicit repo is preserved. Manual code review of the diff confirms no other command's `Field::text(..., required: true)` repo field changed.
  QA scenarios (name the exact tool + invocation): Happy: `cargo test -p cih-engine tui -- --nocapture 2>&1 | tee .omo/evidence/task-3-analyze-cwd-repo.md` exits 0 if tests are filterable; otherwise use the exact new test names. Failure: if private TUI helpers make integration tests awkward, add a focused `#[cfg(test)]` module inside `tui.rs`, rerun the exact command, and record the final test names in evidence.
  Commit: Y | `test(tui): omit analyze repo when blank`

- [x] 4. Workspace verification: Run formatting, targeted tests, clippy, and change impact review - expect clean outputs and scoped diff
  What to do / Must NOT do: Run final formatting, tests, and clippy. Run `git diff -- crates/cih-engine/src/main.rs crates/cih-engine/src/tui.rs crates/cih-engine/Cargo.toml` and confirm no unintended docs/dependency/other-command changes. Run GitNexus `detect_changes({scope:'all'})` after edits and record changed symbols/affected flows. Do not commit unless the user explicitly requests a commit.
  Parallelization: Wave 3 | Blocked by: 2, 3 | Blocks: final verification
  References (executor has NO interview context - be exhaustive): AGENTS.md requires `detect_changes()` before committing and impact analysis before edits; `.omo/drafts/analyze-cwd-repo.md` records approved behavior and scope; `crates/cih-engine/Cargo.toml:49-50` should remain unchanged unless justified; dirty worktree initially contained unrelated `.omo/run-continuation/ses_0e80ae5e8ffeF0dVMOV4vXzbNb.json` and must remain out of scope.
  Acceptance criteria (agent-executable): `cargo fmt --all -- --check`, `cargo test -p cih-engine`, and `cargo clippy -p cih-engine -- -D warnings` all exit 0. Diff contains only intended source/test changes. `detect_changes({scope:'all'})` reports expected symbols only.
  QA scenarios (name the exact tool + invocation): Happy: run `cargo fmt --all -- --check 2>&1 | tee .omo/evidence/task-4-analyze-cwd-repo.md`, then append `cargo test -p cih-engine 2>&1`, then append `cargo clippy -p cih-engine -- -D warnings 2>&1`; all exit 0. Failure: fix the minimal failing code/test and rerun the full sequence, appending the passing rerun to the same evidence file.
  Commit: N | no commit unless explicitly requested

## Final verification wave

> Runs in parallel after ALL todos. ALL must APPROVE. Surface results and wait for the user's explicit okay before declaring complete.

- [x] F1. Plan compliance audit
  - Verify implemented behavior matches this plan: only `analyze` accepts omitted repo, explicit repo still works, missing scope does not imply `--all`, and no other command's repo parsing changed.
  - Evidence: `.omo/evidence/f1-analyze-cwd-repo.md` with diff excerpts and test references.
- [x] F2. Code quality review
  - Review for unnecessary refactors, dependency additions, `unwrap`/`expect` in production path, weak error context, broad CLI side effects, and Rust idiom issues.
  - Evidence: `.omo/evidence/f2-analyze-cwd-repo.md`.
- [x] F3. Real manual QA
  - Build or run the binary in an isolated temp repo and exercise: `cih-engine analyze --all --no-load` from inside repo; `cih-engine analyze <repo> --all --no-load` from outside repo; `cih-engine analyze` from inside repo with no scope and no `cih.scope.toml` to observe existing scope prompt/exit.
  - Evidence: `.omo/evidence/f3-analyze-cwd-repo.md` with exact commands, cwd, exit codes, and selected output.
- [x] F4. Scope fidelity
  - Confirm dirty worktree contains no unrelated changes, especially no `.omo/run-continuation/*` edits staged or included; confirm README/docs unchanged unless explicitly justified.
  - Evidence: `.omo/evidence/f4-analyze-cwd-repo.md` with `git status --short` and scoped `git diff --stat`.

## Commit strategy

- No commit by default.
- If the user later requests a commit, make one atomic commit after final verification passes.
- Suggested message: `feat(cli): default analyze repo to cwd`.
- Before committing, inspect `git status`, `git diff`, and `git log --oneline -10`; stage only intended source/test files and `.omo/evidence` only if the repo convention tracks evidence artifacts.

## Success criteria

- `cih-engine analyze --all` from inside a valid repo starts analysis using cwd.
- `cih-engine analyze /path/to/repo --all` still starts analysis for the explicit repo path.
- `cih-engine analyze` from inside a valid repo without selector/scope preserves the existing scope prompt and exit behavior; it does not index all files by default.
- TUI analyze command assembly omits blank repo and preserves explicit repo.
- `cargo fmt --all -- --check`, `cargo test -p cih-engine`, and `cargo clippy -p cih-engine -- -D warnings` pass.
- GitNexus change detection and final verification show only the intended CLI/TUI/test blast radius.
