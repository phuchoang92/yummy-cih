# Interactive Start for yummy-cih

## TL;DR
> **Summary**: Add a guided `cih-engine start` wizard that configures `.env`, checks prerequisites, and optionally runs Docker/index/wiki/docs commands with explicit confirmations. Update README and Docker docs so the interactive path becomes the default Quick Start.
> **Deliverables**:
> - `cih-engine start` interactive CLI wizard
> - Safe `.env` create/update behavior in the CIH project root
> - Optional LLM provider setup without requiring API keys
> - Tests-after coverage for pure wizard planning/env logic
> - README, Docker Quickstart, and docs-viewer deployment doc updates
> **Effort**: Medium
> **Parallel**: YES - 3 waves
> **Critical Path**: Task 1 → Task 2 → Task 4 → Task 5 → Task 8

## Context
### Original Request
User: "seem the quick start is not so interactive. Can you make interactive start?"

### Interview Summary
- Interaction form: **both** CLI wizard and README/docs update.
- Primary command: `cih-engine start`.
- Automation level: **guided execute** — prompt, write `.env`, optionally run Docker/index/wiki/docs commands after explicit confirmations.
- LLM setup: **optional prompt**; user may skip.
- Test strategy: **tests-after**.
- `.env` location: CIH project root next to `docker-compose.yml`, not the target Java repository root.

### Metis Review (gaps addressed)
- Docker Compose cannot bootstrap the host `.env` through `docker compose run engine start`, because Compose loads `${REPO_PATH}` before the service starts. Plan resolves this by making `cih-engine start` the native/installed wizard path and documenting Docker-safe fallback/manual commands.
- `docs/DOCKER-QUICKSTART.md` also needs update, not only README.
- No existing stdin wizard pattern exists; isolate prompt-free planning/env functions for tests.
- `.env` is ignored by git and scan behavior; do not read or modify target repo `.env` files.

## Work Objectives
### Core Objective
Make first-run setup interactive and safe by adding `cih-engine start` as a guided wizard for configuring and launching CIH against a Java/Spring repository.

### Deliverables
- New `crates/cih-engine/src/start.rs` module.
- New `crates/cih-engine/src/start_env.rs` module for `.env` rendering and safe writes.
- `dialoguer` dependency in `crates/cih-engine/Cargo.toml`.
- `Start` variant in `crates/cih-engine/src/main.rs`.
- Tests for env rendering, path validation, command-plan generation, existing `.env` preservation, and non-interactive validation.
- Documentation updates in `README.md`, `docs/DOCKER-QUICKSTART.md`, and `docs-viewer/DEPLOY.md`.

### Definition of Done (verifiable conditions with commands)
- `cargo test --workspace` passes.
- `cargo run -p cih-engine -- start --dry-run --repo /tmp/cih-missing-repo` exits non-zero with a clear path error.
- `cargo run -p cih-engine -- start --dry-run --non-interactive --repo <fixture-java-repo> --repo-name demo` prints a command plan and does not write `.env`.
- `cd docs-viewer && npm run build` passes.
- README Quick Start points to `cih-engine start` first and preserves a manual Docker fallback.

### Must Have
- Wizard writes `.env` only in the CIH workspace root beside `docker-compose.yml`.
- Existing `.env` unknown keys and comments are preserved; updated keys are `REPO_PATH`, `REPO_NAME`, and optional LLM provider key only.
- Long-running commands require explicit confirmation one-by-one.
- `--dry-run` never writes files and never runs commands.
- API keys are never printed back to terminal output or test logs.

### Must NOT Have
- Do not rely on `docker compose run engine start` before `.env` exists.
- Do not edit `docker-compose.yml` unless implementation discovers it is strictly required; current plan assumes no compose changes.
- Do not write `.env` into the target Java repository.
- Do not run destructive Docker commands such as `docker compose down -v`.
- Do not add CI configuration in this feature.

## Verification Strategy
> ZERO HUMAN INTERVENTION - all verification is agent-executed.
- Test decision: **tests-after** using Rust unit tests plus docs-viewer build/smoke validation.
- QA policy: Every task has agent-executed scenarios.
- Evidence: `.omo/evidence/task-{N}-{slug}.{ext}`

## Execution Strategy
### Parallel Execution Waves
> Target: 5-8 tasks per wave. <3 per wave (except final) = under-splitting.
> Extract shared dependencies as Wave-1 tasks for max parallelism.

Wave 1: Task 1 safety/impact, Task 2 wizard core model, Task 3 env-file behavior.
Wave 2: Task 4 CLI wiring/prompts, Task 5 guided command execution, Task 6 documentation.
Wave 3: Task 7 docs-viewer validation docs, Task 8 integrated verification and polish.

### Dependency Matrix (full, all tasks)
- Task 1 blocks Tasks 2-8.
- Task 2 blocks Tasks 4, 5, 8.
- Task 3 blocks Tasks 4, 8.
- Task 4 blocks Tasks 5, 8.
- Task 5 blocks Task 8.
- Task 6 blocks Task 8.
- Task 7 blocks Task 8.
- Task 8 blocks final verification wave.

### Agent Dispatch Summary
- Wave 1 → 3 tasks → categories: `quick`, `unspecified-high`, `quick`
- Wave 2 → 3 tasks → categories: `unspecified-high`, `unspecified-high`, `writing`
- Wave 3 → 2 tasks → categories: `writing`, `unspecified-high`

## TODOs
> Implementation + Test = ONE task. Never separate.
> EVERY task MUST have: Agent Profile + Parallelization + QA Scenarios.

- [x] 1. Safety and Impact Baseline

  **What to do**: Create `.omo/evidence/`, run GitNexus impact analysis before code edits for `main`/`Command` in `crates/cih-engine/src/main.rs`, inspect current diff, and record risk in implementation notes. If impact is HIGH or CRITICAL, warn the user before editing. Do not modify files in this task except optional evidence notes.
  **Must NOT do**: Do not edit symbols before impact analysis. Do not commit.

  **Recommended Agent Profile**:
  - Category: `quick` - Reason: focused safety gate before implementation.
  - Skills: [`gitnexus/gitnexus-impact-analysis`] - Required by repository rules before symbol edits.
  - Omitted: [`playwright`] - No browser/UI work in this task.

  **Parallelization**: Can Parallel: NO | Wave 1 | Blocks: Tasks 2-8 | Blocked By: none

  **References**:
  - Pattern: `AGENTS.md` - requires impact analysis before editing symbols.
  - Pattern: `crates/cih-engine/src/main.rs:27-35` - `Cli` and `Command` definitions to be changed later.
  - Pattern: `crates/cih-engine/src/main.rs:300-474` - command dispatch match to be changed later.

  **Acceptance Criteria**:
  - [ ] `gitnexus_impact` or equivalent GitNexus impact result for `Command`/`main` is saved in `.omo/evidence/task-1-impact-baseline.md`.
  - [ ] `git status --short` shows no source/doc changes from this task.

  **QA Scenarios**:
  ```
  Scenario: Impact baseline recorded
    Tool: Bash + GitNexus
    Steps: Run impact analysis, then inspect .omo/evidence/task-1-impact-baseline.md
    Expected: Evidence file includes risk level, direct callers/importers, and affected processes or states none found
    Evidence: .omo/evidence/task-1-impact-baseline.md

  Scenario: No accidental mutation
    Tool: Bash
    Steps: Run git status --short
    Expected: No source/doc files changed by Task 1
    Evidence: .omo/evidence/task-1-git-status.txt
  ```

  **Commit**: NO | Message: n/a | Files: [.omo/evidence/task-1-*]

- [x] 2. Wizard Core Model and Command Plan

  **What to do**: Add `dialoguer = "0.11"` to `crates/cih-engine/Cargo.toml`. Create `crates/cih-engine/src/start.rs` with prompt-free core types and functions: `StartConfig`, `LlmChoice`, `IndexMode`, `PlannedCommand`, `validate_repo_path`, `default_repo_name`, and `build_command_plan`. The command plan must include exact Docker Compose commands for: `pull`, `up -d`, `ps`, optional `scan`, `analyze`, `discover`, optional `embed`, optional `wiki`, optional docs viewer `docker compose --profile docs up -d docs-viewer`.
  **Must NOT do**: Do not add stdin prompts in this task. Do not shell out to Docker in pure functions.

  **Recommended Agent Profile**:
  - Category: `unspecified-high` - Reason: introduces new core module and testable abstractions.
  - Skills: [`gitnexus/gitnexus-impact-analysis`] - Needed if touching `main.rs` beyond module declaration.
  - Omitted: [`playwright`] - No browser interaction.

  **Parallelization**: Can Parallel: YES | Wave 1 | Blocks: Tasks 4, 5, 8 | Blocked By: Task 1

  **References**:
  - Pattern: `crates/cih-engine/Cargo.toml:11-36` - dependency style; `ureq` is direct dependency.
  - Pattern: `crates/cih-engine/src/wiki_cmd.rs:35-61` - config struct style to follow.
  - Pattern: `crates/cih-engine/src/wiki_cmd.rs:63-93` - default config implementation pattern.
  - API/Type: `crates/cih-engine/src/main.rs:34-240` - subcommand options should eventually map into config types.
  - External: `https://docs.rs/dialoguer/latest/dialoguer/` - prompt library reference.

  **Acceptance Criteria**:
  - [ ] `cargo test -p cih-engine start_` passes and covers pure functions.
  - [ ] LLM choice `None` produces no API key line.
  - [ ] LLM choices produce only the selected provider key placeholder/key name, not all providers.
  - [ ] `build_command_plan` never includes `docker compose down -v`.

  **QA Scenarios**:
  ```
  Scenario: Happy path command plan
    Tool: Bash
    Steps: Run cargo test -p cih-engine start_builds_full_command_plan -- --nocapture
    Expected: Test passes and verifies pull/up/ps/analyze/discover/wiki/docs command order
    Evidence: .omo/evidence/task-2-command-plan.txt

  Scenario: Invalid repo path rejected
    Tool: Bash
    Steps: Run cargo test -p cih-engine start_rejects_missing_repo_path -- --nocapture
    Expected: Test passes and error message contains "repository path does not exist"
    Evidence: .omo/evidence/task-2-invalid-path.txt
  ```

  **Commit**: NO | Message: `feat(engine): add interactive start planning core` | Files: [`crates/cih-engine/Cargo.toml`, `Cargo.lock`, `crates/cih-engine/src/start.rs`]

- [x] 3. Safe `.env` Create/Update Behavior

  **What to do**: Create `crates/cih-engine/src/start_env.rs` and implement prompt-free `.env` handling helpers: `render_env`, `load_env_file`, `merge_env_values`, `write_env_file`. Merge behavior must preserve comments, blank lines, and unknown keys; update only `REPO_PATH`, `REPO_NAME`, and selected optional LLM key. If `.env` exists, write a timestamped backup `.env.cih-backup-YYYYMMDDHHMMSS` before overwriting. If `--dry-run` is active, return the would-write content but write nothing.
  **Must NOT do**: Do not parse or modify `.env` files inside the target Java repository. Do not print API key values.

  **Recommended Agent Profile**:
  - Category: `quick` - Reason: contained file helper logic with tests.
  - Skills: [] - No specialized skill needed beyond Rust testing.
  - Omitted: [`gitnexus/gitnexus-refactoring`] - New helper logic, not a rename/refactor.

  **Parallelization**: Can Parallel: YES | Wave 1 | Blocks: Tasks 4, 8 | Blocked By: Task 1

  **References**:
  - Pattern: `.gitignore:8-11` - `.env` and `.env.*` are ignored, `.env.example` is allowed.
  - Pattern: `docker-compose.yml:29` - `${REPO_PATH}` mount for server.
  - Pattern: `docker-compose.yml:55` - `${REPO_NAME:-repo}` docs viewer mount naming.
  - Pattern: `README.md:20-29` - README says `.env` next to `docker-compose.yml` with required `REPO_PATH`.

  **Acceptance Criteria**:
  - [ ] `render_env` output contains `REPO_PATH=<absolute path>` and `REPO_NAME=<name>`.
  - [ ] Tests prove new `.env` file contains `REPO_PATH` and `REPO_NAME`.
  - [ ] Tests prove comments and unrelated keys survive update.
  - [ ] Tests prove existing `.env` creates one `.env.cih-backup-*` file.
  - [ ] Tests prove dry-run creates no `.env` and no backup.

  **QA Scenarios**:
  ```
  Scenario: Existing env preserved
    Tool: Bash
    Steps: Run cargo test -p cih-engine start_preserves_existing_env_comments -- --nocapture
    Expected: Test passes; unrelated FOO=bar and comments remain
    Evidence: .omo/evidence/task-3-preserve-env.txt

  Scenario: Dry run writes nothing
    Tool: Bash
    Steps: Run cargo test -p cih-engine start_dry_run_writes_no_env_file -- --nocapture
    Expected: Test passes; temp workspace has no .env and no backup
    Evidence: .omo/evidence/task-3-dry-run.txt
  ```

  **Commit**: NO | Message: `feat(engine): safely write start wizard env file` | Files: [`crates/cih-engine/src/start_env.rs`]

- [x] 4. Wire `cih-engine start` CLI and Interactive Prompts

  **What to do**: Add `mod start;` and `mod start_env;` in `crates/cih-engine/src/main.rs`. Add `Command::Start` with flags: `--workspace <path>` default current directory, `--repo <path>` optional prefill, `--repo-name <name>` optional prefill, `--dry-run`, and `--non-interactive`. Wire dispatch to `start::run_start`. Interactive prompt order must be: workspace validation → repo path → repo name → index mode (`scan-only`, `analyze-all`, `modules`) → modules if chosen → discover yes/no → embed yes/no → wiki yes/no → docs viewer yes/no → LLM provider optional → API key secret prompt only when provider needs a key → final summary → confirmation to write `.env` → per-command confirmations.
  **Must NOT do**: Do not add `--yes` full-auto behavior. Do not run commands before final summary and explicit confirmation.

  **Recommended Agent Profile**:
  - Category: `unspecified-high` - Reason: CLI integration plus interactive behavior.
  - Skills: [`gitnexus/gitnexus-impact-analysis`] - Required before editing `Command`/dispatch symbols.
  - Omitted: [`playwright`] - CLI work only.

  **Parallelization**: Can Parallel: YES | Wave 2 | Blocks: Tasks 5, 8 | Blocked By: Tasks 2, 3

  **References**:
  - Pattern: `crates/cih-engine/src/main.rs:1-15` - module declarations.
  - Pattern: `crates/cih-engine/src/main.rs:27-35` - clap `Cli` and `Command` derive style.
  - Pattern: `crates/cih-engine/src/main.rs:419-472` - command dispatch into config object.
  - Pattern: `crates/cih-engine/src/tests.rs:14-40` - temp repo helper style for tests.
  - External: `https://docs.rs/dialoguer/latest/dialoguer/struct.Input.html` and `Password`/`Confirm` docs.

  **Acceptance Criteria**:
  - [ ] `cargo run -p cih-engine -- start --help` lists `start` options and exits 0.
  - [ ] `cargo run -p cih-engine -- start --dry-run --repo <fixture> --repo-name demo --non-interactive` exits 0 and prints summary without writing `.env`.
  - [ ] `cargo run -p cih-engine -- start --non-interactive` without `--repo` exits non-zero with a clear missing repo error.
  - [ ] API key prompt uses a hidden/secret input path and no key appears in normal output.

  **QA Scenarios**:
  ```
  Scenario: Non-interactive dry-run happy path
    Tool: Bash
    Steps: Create temp Java fixture; run cargo run -p cih-engine -- start --dry-run --non-interactive --repo <fixture> --repo-name demo
    Expected: Exit 0; output includes REPO_PATH, REPO_NAME, planned docker compose commands; workspace .env absent
    Evidence: .omo/evidence/task-4-dry-run.txt

  Scenario: Missing non-interactive repo fails
    Tool: Bash
    Steps: Run cargo run -p cih-engine -- start --non-interactive
    Expected: Exit non-zero; stderr includes "--repo is required in --non-interactive mode"
    Evidence: .omo/evidence/task-4-missing-repo.txt
  ```

  **Commit**: NO | Message: `feat(engine): add cih-engine start wizard` | Files: [`crates/cih-engine/src/main.rs`, `crates/cih-engine/src/start.rs`, `crates/cih-engine/src/start_env.rs`]

- [x] 5. Guided Command Execution and Preflight Checks

  **What to do**: Implement command execution in `start.rs` behind a small `CommandRunner` abstraction. Preflight checks must verify: workspace contains `docker-compose.yml`, Docker CLI exists, `docker compose version` succeeds, target repo path exists, and target repo has at least one `.java` file or a clear warning if none. After `.env` write, ask before each command: `docker compose pull`, `docker compose up -d`, `docker compose ps`, optional scan/analyze/discover/embed/wiki/docs viewer. If user declines a command, print the exact skipped command for copy/paste.
  **Must NOT do**: Do not run commands in `--dry-run`. Do not run Docker commands if Docker preflight fails. Do not hide failing command exit statuses.

  **Recommended Agent Profile**:
  - Category: `unspecified-high` - Reason: process execution and failure handling.
  - Skills: [] - General Rust/process work.
  - Omitted: [`playwright`] - No browser test required for CLI execution.

  **Parallelization**: Can Parallel: NO | Wave 2 | Blocks: Task 8 | Blocked By: Task 4

  **References**:
  - Pattern: `README.md:31-70` - canonical Docker/index command sequence.
  - Pattern: `README.md:76-123` - wiki and optional LLM command examples.
  - Pattern: `docker-compose.yml:60-76` - engine service is one-shot under tools profile.
  - Pattern: `docs/DOCKER-QUICKSTART.md:76-210` - Docker command sequence to preserve as fallback.

  **Acceptance Criteria**:
  - [ ] Unit tests using a fake runner verify commands are requested in the expected order.
  - [ ] Declined commands are printed as copy/paste snippets.
  - [ ] Docker preflight failure stops command execution but still allows `.env` dry-run summary.
  - [ ] Command failure returns non-zero and names the failed command.

  **QA Scenarios**:
  ```
  Scenario: User declines analyze command
    Tool: Bash
    Steps: Run cargo test -p cih-engine start_declined_command_prints_copy_paste -- --nocapture
    Expected: Test passes; skipped analyze command is printed exactly once
    Evidence: .omo/evidence/task-5-decline-command.txt

  Scenario: Docker unavailable failure
    Tool: Bash
    Steps: Run cargo test -p cih-engine start_docker_preflight_failure_blocks_execution -- --nocapture
    Expected: Test passes; no command runner invocations after failed preflight
    Evidence: .omo/evidence/task-5-docker-preflight.txt
  ```

  **Commit**: NO | Message: `feat(engine): execute start wizard steps safely` | Files: [`crates/cih-engine/src/start.rs`]

- [x] 6. README and Docker Quickstart Updates

  **What to do**: Update `README.md` Quick Start so Step 1 introduces `cih-engine start` as the recommended interactive path. Keep a manual `.env` fallback immediately below it. Add a short note that the wizard is native/installed and that Docker Compose cannot run it before `.env` exists. Update `docs/DOCKER-QUICKSTART.md` with the same distinction: interactive native wizard first when available, manual Docker-only fallback for users who only have Docker.
  **Must NOT do**: Do not remove existing manual commands; keep them as fallback. Do not claim `docker compose run --rm engine start` works before `.env` exists.

  **Recommended Agent Profile**:
  - Category: `writing` - Reason: documentation rewrite with technical guardrails.
  - Skills: [] - No code skills needed.
  - Omitted: [`gitnexus/gitnexus-refactoring`] - Documentation only.

  **Parallelization**: Can Parallel: YES | Wave 2 | Blocks: Task 8 | Blocked By: Task 1

  **References**:
  - Pattern: `README.md:18-29` - current `.env` setup step.
  - Pattern: `README.md:31-70` - current Docker start/index sequence.
  - Pattern: `README.md:96-127` - LLM provider examples to summarize from wizard path.
  - Pattern: `docs/DOCKER-QUICKSTART.md:14-32` - current workspace and compose setup.
  - Pattern: `docs/DOCKER-QUICKSTART.md:111-210` - current Docker-only analyze/list commands.

  **Acceptance Criteria**:
  - [ ] README Quick Start starts with `cih-engine start` and explains what it prompts for.
  - [ ] README includes manual `.env` fallback with `REPO_PATH=/absolute/path/to/your/java-project`.
  - [ ] `docs/DOCKER-QUICKSTART.md` clearly separates "Interactive path" and "Docker-only manual path".
  - [ ] No docs mention unsupported `docker compose run --rm engine start` as bootstrap.

  **QA Scenarios**:
  ```
  Scenario: README contains interactive path
    Tool: Bash
    Steps: Search README.md for "cih-engine start" and "manual fallback"
    Expected: Both phrases/sections exist; Quick Start still contains REPO_PATH fallback
    Evidence: .omo/evidence/task-6-readme-check.txt

  Scenario: Docker doc avoids compose self-bootstrap
    Tool: Bash
    Steps: Search docs/DOCKER-QUICKSTART.md for "docker compose run --rm engine start"
    Expected: No match; document includes Docker-only fallback commands
    Evidence: .omo/evidence/task-6-docker-doc-check.txt
  ```

  **Commit**: NO | Message: `docs: document interactive start wizard` | Files: [`README.md`, `docs/DOCKER-QUICKSTART.md`]

- [x] 7. Docs Viewer Deployment Notes

  **What to do**: Update `docs-viewer/DEPLOY.md` to mention that generated wiki docs can now be prepared through `cih-engine start`, then preserve existing direct `cih-engine analyze/discover/wiki` and Docker Compose examples as fallback. If no docs-viewer code changes are needed, do not modify `docs-viewer/scripts/gen-index.js` or `docusaurus.config.js`.
  **Must NOT do**: Do not change docs-viewer runtime behavior for this feature unless tests prove documentation alone is insufficient.

  **Recommended Agent Profile**:
  - Category: `writing` - Reason: deployment docs update.
  - Skills: [] - Documentation only.
  - Omitted: [`playwright`] - No visual/UI changes.

  **Parallelization**: Can Parallel: YES | Wave 3 | Blocks: Task 8 | Blocked By: Task 1

  **References**:
  - Pattern: `docs-viewer/DEPLOY.md:43-58` - current docs generation commands.
  - Pattern: `docs-viewer/DEPLOY.md:70-116` - current docs viewer run commands.
  - Pattern: `docs-viewer/package.json:5-9` - `start`, `build`, `serve` scripts.
  - Pattern: `docs-viewer/docusaurus.config.js:8-18` - single/multi repo mode detection.

  **Acceptance Criteria**:
  - [ ] `docs-viewer/DEPLOY.md` mentions `cih-engine start` as an optional preparation path.
  - [ ] Existing direct and Docker commands remain available.
  - [ ] `cd docs-viewer && npm run build` passes.

  **QA Scenarios**:
  ```
  Scenario: Docs viewer build still works
    Tool: Bash
    Steps: cd docs-viewer && npm run build
    Expected: Exit 0
    Evidence: .omo/evidence/task-7-docs-viewer-build.txt

  Scenario: Deploy guide preserves fallback
    Tool: Bash
    Steps: Search docs-viewer/DEPLOY.md for "cih-engine analyze" and "cih-engine start"
    Expected: Both are present
    Evidence: .omo/evidence/task-7-deploy-doc-check.txt
  ```

  **Commit**: NO | Message: `docs(viewer): reference interactive start flow` | Files: [`docs-viewer/DEPLOY.md`]

- [x] 8. Integrated Verification and Polish

  **What to do**: Run the complete validation suite and fix all failures introduced by Tasks 2-7. Commands: `cargo fmt --check`, `cargo test --workspace`, `cargo run -p cih-engine -- start --help`, dry-run happy path with a temp Java fixture, dry-run missing repo failure, and `cd docs-viewer && npm run build`. Run GitNexus `detect_changes({scope:"all"})` before any commit or final review.
  **Must NOT do**: Do not commit unless explicitly requested by the user. Do not skip failing docs-viewer build because it is "unrelated" without proving it existed before.

  **Recommended Agent Profile**:
  - Category: `unspecified-high` - Reason: full integration verification and fixes.
  - Skills: [`gitnexus/gitnexus-impact-analysis`] - Detect changes before commit/review.
  - Omitted: [`playwright`] - Browser automation not needed unless docs-viewer runtime smoke is chosen.

  **Parallelization**: Can Parallel: NO | Wave 3 | Blocks: Final Verification | Blocked By: Tasks 2-7

  **References**:
  - Test: `crates/cih-engine/src/tests.rs:49-96` - existing Rust test style.
  - Test: `docs-viewer/package.json:5-9` - docs-viewer build command.
  - Pattern: `README.md:337` - existing `cargo test --workspace` command.
  - Pattern: `AGENTS.md` - requires `detect_changes()` before committing.

  **Acceptance Criteria**:
  - [ ] `cargo fmt --check` passes.
  - [ ] `cargo test --workspace` passes.
  - [ ] `cargo run -p cih-engine -- start --help` exits 0 and shows start options.
  - [ ] Dry-run happy path exits 0 and writes no `.env`.
  - [ ] Missing repo path dry-run exits non-zero with clear error.
  - [ ] `cd docs-viewer && npm run build` passes.
  - [ ] GitNexus `detect_changes({scope:"all"})` output is saved to `.omo/evidence/task-8-detect-changes.md`.

  **QA Scenarios**:
  ```
  Scenario: Full regression suite
    Tool: Bash
    Steps: Run cargo fmt --check && cargo test --workspace
    Expected: Exit 0
    Evidence: .omo/evidence/task-8-rust-regression.txt

  Scenario: Start command dry-run behavior
    Tool: Bash
    Steps: Create temp Java fixture; run cargo run -p cih-engine -- start --dry-run --non-interactive --repo <fixture> --repo-name demo; verify no .env in CIH root
    Expected: Exit 0; output includes command plan; .env absent or unchanged
    Evidence: .omo/evidence/task-8-start-dry-run.txt
  ```

  **Commit**: NO | Message: `feat(engine): add interactive start wizard` | Files: [`crates/cih-engine/src/main.rs`, `crates/cih-engine/src/start.rs`, `crates/cih-engine/src/start_env.rs`, `crates/cih-engine/Cargo.toml`, `Cargo.lock`, `README.md`, `docs/DOCKER-QUICKSTART.md`, `docs-viewer/DEPLOY.md`]

## Final Verification Wave (MANDATORY — after ALL implementation tasks)
> 4 review agents run in PARALLEL. ALL must APPROVE. Present consolidated results to user and get explicit "okay" before completing.
> **Do NOT auto-proceed after verification. Wait for user's explicit approval before marking work complete.**
> **Never mark F1-F4 as checked before getting user's okay.** Rejection or user feedback -> fix -> re-run -> present again -> wait for okay.
- [x] F1. Plan Compliance Audit — oracle
- [x] F2. Code Quality Review — unspecified-high
- [x] F3. Real Manual QA — unspecified-high
- [x] F4. Scope Fidelity Check — deep

## Commit Strategy
- Default: do not commit unless user explicitly requests.
- If asked to commit, use one commit after Task 8 passes.
- Suggested message: `feat(engine): add interactive start wizard`
- Before commit: inspect `git status`, `git diff`, `git log --oneline -10`, and run GitNexus `detect_changes({scope:"all"})`.

## Success Criteria
- New users can run `cih-engine start` and receive an interactive, safe setup path.
- Manual Docker Quick Start remains available and accurate.
- `.env` handling is safe, reversible, and scoped to CIH workspace root.
- Long-running commands are never executed without explicit confirmation.
- Regression tests and docs-viewer build pass.
