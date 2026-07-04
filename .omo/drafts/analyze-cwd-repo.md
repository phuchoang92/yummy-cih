---
slug: analyze-cwd-repo
status: drafting
intent: clear
pending-action: write .omo/plans/analyze-cwd-repo.md
approach: make only `cih-engine analyze` accept an omitted positional repo path by resolving it to the current working directory before dispatch; preserve explicit repo paths and existing scope-selection behavior.
---

# Draft: analyze-cwd-repo

## Components (topology ledger)
<!-- Lock the SHAPE before depth. One row per top-level component that can succeed or fail independently. -->
<!-- id | outcome (one line) | status: active|deferred | evidence path -->

| C1 | CLI parse surface: `Command::Analyze` repo positional becomes optional while explicit path still parses | active | `crates/cih-engine/src/main.rs:67-103`; Context7 clap optional positional docs |
| C2 | Analyze dispatch: omitted repo resolves to `std::env::current_dir()` before existing `run_analyze(PathBuf, flags)` | active | `crates/cih-engine/src/main.rs:521-553`; `crates/cih-engine/src/analyze/mod.rs:42-120` |
| C3 | TUI command builder: analyze repo field no longer emits `<repo>` when blank | active | `crates/cih-engine/src/tui.rs:135-145`, `crates/cih-engine/src/tui.rs:193-207`, `crates/cih-engine/src/tui.rs:321-336` |
| C4 | Tests and verification: parser/TUI/runtime behavior locked with agent-executable commands | active | `crates/cih-engine/Cargo.toml:49-50`; `crates/cih-engine/tests/crate_tests.rs`; `crates/cih-engine/tests/start.rs` |

## Open assumptions (announced defaults)
<!-- Record any default you adopt instead of asking, so the user can veto it at the gate. -->
<!-- assumption | adopted default | rationale | reversible? -->

| Missing analyze repo path | Use current working directory | User explicitly requested running `cih-engine analyze` while standing on the repo | yes |
| Missing analyze scope | Preserve current behavior; do not imply `--all` | Avoid surprising full-repo indexing on large repos; user approved this behavior | yes |
| Other commands with repo args | Leave unchanged | Request named only `analyze`; Metis flagged scope-creep risk | yes |
| README docs | Out of scope unless already touched by generated help/test needs | User requested command behavior, not docs refresh | yes |

## Findings (cited - path:lines)

- `Command::Analyze` currently requires positional `repo: PathBuf`, so clap rejects `cih-engine analyze` before dispatch: `crates/cih-engine/src/main.rs:67-70`.
- Analyze dispatch passes the positional `repo` directly into `analyze::run_analyze(repo, AnalyzeFlags { ... })`: `crates/cih-engine/src/main.rs:521-553`.
- `run_analyze(repo: PathBuf, flags: AnalyzeFlags)` should keep receiving an owned `PathBuf`; it already delegates path validation to scan: `crates/cih-engine/src/analyze/mod.rs:42-49`.
- `scan::scan_repo` canonicalizes the path and fails if it cannot resolve: `crates/cih-engine/src/scan.rs:128-132`.
- Scope selection must stay unchanged: `run_analyze` exits with the existing prompt when `request.has_selector()` is false: `crates/cih-engine/src/analyze/mod.rs:57-64`; selector logic lives in `crates/cih-engine/src/scope.rs:42-44`.
- TUI blank optional text fields are omitted, while required blank fields emit placeholders: `crates/cih-engine/src/tui.rs:135-145`; assembled commands include all fragments from `to_shell_fragment`: `crates/cih-engine/src/tui.rs:321-336`.
- TUI analyze repo metadata is currently required and must be changed to optional: `crates/cih-engine/src/tui.rs:193-207`.
- Cargo dev-dependencies currently include only `tempfile`, so parser tests should avoid introducing `assert_cmd` unless absolutely necessary: `crates/cih-engine/Cargo.toml:49-50`.

## Decisions (with rationale)

- Approved by user: missing analyze repo path defaults to current working directory; missing scope does not imply `--all`.
- Implement repo default in `main.rs` dispatch, not inside `run_analyze`, so analyzer internals keep one resolved `PathBuf` input and explicit path behavior remains unchanged.
- Add context to `std::env::current_dir()` failure so deleted/invalid CWD errors tell users to pass an explicit repo path or run from a valid directory.
- Update TUI metadata for analyze only; do not change Scan/Resolve/Discover/Embed/Wiki repo argument requirements.
- Use tests-after strategy with parser/unit tests plus targeted `cargo test -p cih-engine` and `cargo clippy -p cih-engine -- -D warnings`.

## Scope IN

- `crates/cih-engine/src/main.rs`: analyze CLI parser and dispatch resolution.
- `crates/cih-engine/src/tui.rs`: analyze command-builder metadata and tests for omitted/explicit repo fragments if current test visibility allows.
- Test files or `#[cfg(test)]` modules needed to lock parser and TUI behavior.
- `.omo/evidence/`: command logs created by the executor.

## Scope OUT (Must NOT have)

- Do not make `cih-engine analyze` imply `--all`.
- Do not change `run_analyze`, `scan_repo`, or other analyzer pipeline signatures unless tests prove unavoidable.
- Do not change `Scan`, `Resolve`, `Discover`, `Embed`, `Wiki`, `Taint`, or artifact command repo arguments.
- Do not add broad CLI dependencies such as `assert_cmd` unless a lighter parser/unit test path cannot work.
- Do not modify README or user docs unless a compiler/test failure requires it.
- Do not touch unrelated untracked `.omo/run-continuation/*` files.

## Open questions

None. User approved the default behavior on 2026-06-30.

## Approval gate
status: approved
<!-- When exploration is exhausted and unknowns are answered, set status: awaiting-approval. -->
<!-- That durable record is the loop guard: on a later turn read it and resume at the gate instead of re-running exploration. -->

Approved action: write `.omo/plans/analyze-cwd-repo.md` only. Execution remains separate and starts only if the user chooses `$start-work` or equivalent.

## Metis review folded in

- Added explicit exclusions for other commands to prevent scope creep.
- Added TUI command fragment requirements: blank optional repo must be omitted, explicit repo must be included.
- Added exact parser/runtime/TUI test expectations and command-level verification.
- Added current working directory error-context requirement.
- Clarified that `run_analyze` and `scan_repo` signatures remain unchanged.
