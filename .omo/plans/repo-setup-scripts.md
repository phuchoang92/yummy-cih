# repo-setup-scripts - Work Plan

## TL;DR (For humans)
<!-- Fill this LAST, after the detailed plan below is written, so it summarizes the REAL plan. -->
<!-- Plain English for a non-engineer: NO file paths, NO todo numbers, NO wave/agent/tool names. -->

**What you'll get:** A friendly first-run setup experience from the repository root for both macOS/Linux and Windows. Users can choose either to build local binaries and put them on PATH, or to bootstrap the Docker Compose stack without manually assembling `.env`.

**Why this approach:** It keeps the entrypoint obvious (`setup.sh` / `setup.bat`) and avoids changing the Rust app for a setup-only request. Binary mode honors your choice to build only, while Docker mode fixes the current `.env` friction.

**What it will NOT do:** It will not install binaries into privileged system folders, add CI, run destructive Docker cleanup, or modify unrelated project files.

**Effort:** Medium
**Risk:** Medium - persistent PATH edits and cross-platform script behavior need careful idempotency checks.
**Decisions to sanity-check:** Binary mode is build-only; PATH is always persisted; Windows target is `setup.bat` for cmd.exe with PowerShell helpers where needed.

Your next move: choose whether to start work now or run a high-accuracy review first. Full execution detail follows below.

---

> TL;DR (machine): Medium-risk docs/scripts work adding root `setup.sh` and `setup.bat`, binary build/PATH mode, Docker Compose bootstrap mode, docs updates, and tests-after QA.

## Scope
### Must have
- Add root-level `setup.sh` for macOS/Linux.
- Add root-level `setup.bat` for Windows cmd.exe users.
- Both scripts must show an interactive top-level choice:
  1. Binary build mode.
  2. Docker Compose mode.
- Binary build mode must:
  - verify `cargo` is available;
  - run `cargo build --release -p cih-server -p cih-engine` from the repo root;
  - verify `target/release/cih-engine` and `target/release/cih-server` exist and can print help/version without crashing;
  - persist the absolute repo `target/release` directory to user PATH idempotently;
  - print the exact commands now available (`cih-engine`, `cih-server`) and a short note that runtime graph/postgres services are still needed for commands that require them.
- Docker Compose mode must:
  - verify Docker Compose v2 and Docker daemon availability;
  - prompt for a valid absolute `REPO_PATH`;
  - prompt for non-empty `POSTGRES_PASSWORD`;
  - offer `REPO_NAME` with a sanitized target-repo-folder default;
  - write/update root `.env` with at least `REPO_PATH`, `POSTGRES_PASSWORD`, `REPO_NAME`, preserving/backup behavior;
  - run `docker compose config` before starting services;
  - run `docker compose pull` and `docker compose up -d`;
  - finish by pointing users to README indexing commands rather than duplicating a long command plan.
- README and `docs/DOCKER-QUICKSTART.md` must make `setup.sh`/`setup.bat` the friendly first-run path and keep manual fallback.
- Every script change must include tests-after QA evidence under `.omo/evidence/`.
### Must NOT have (guardrails, anti-slop, scope boundaries)
- Do not add CI configuration.
- Do not copy binaries into privileged/global directories such as `/usr/local/bin`, `C:\Windows`, or `Program Files`.
- Do not run destructive Docker commands (`docker compose down -v`, volume removal, database wipe).
- Do not write `.env` into the target Java/Spring repository; only write the CIH repo-root `.env`.
- Do not print `POSTGRES_PASSWORD` after input or include it in evidence logs.
- Do not edit Rust symbols unless the worker first runs required GitNexus impact analysis and records the result.
- Do not touch unrelated untracked files already present in the worktree.

## Verification strategy
> Zero human intervention - all verification is agent-executed.
- Test decision: tests-after using Bash syntax checks, script dry-run/helper modes where implemented, temporary HOME/USERPROFILE idempotency checks, Docker Compose config validation, and docs assertions. No external CI is assumed.
- Evidence: .omo/evidence/task-<N>-repo-setup-scripts.<ext>
- Minimum verification commands to include by the final todo:
  - `bash -n setup.sh`
  - `cargo build --release -p cih-server -p cih-engine` or a recorded skip only if environment lacks Rust, with prerequisite failure verified instead
  - Non-destructive PATH idempotency using temporary HOME/USERPROFILE or isolated profile files
  - `.env` generation/backup validation in a temporary copy/workspace, without committing `.env`
  - `docker compose config` against generated temp `.env` if Docker is available; otherwise record prerequisite failure path
  - README/docs grep/assertion checks for setup-script entrypoints and fallback retention

## Execution strategy
### Parallel execution waves
> Target 5-8 todos per wave. Fewer than 3 (except the final) means you under-split.

- Wave 1: safety baseline and shared behavior specification (Todos 1-3).
- Wave 2: implement the two scripts and docs (Todos 4-6).
- Wave 3: integrated cross-platform/idempotency verification and cleanup (Todo 7).

### Dependency matrix
| Todo | Depends on | Blocks | Can parallelize with |
| --- | --- | --- | --- |
| 1 | none | 2-7 | none |
| 2 | 1 | 4, 7 | 3 |
| 3 | 1 | 4, 5, 7 | 2 |
| 4 | 2, 3 | 7 | 5, 6 |
| 5 | 3 | 7 | 4, 6 |
| 6 | 3 | 7 | 4, 5 |
| 7 | 4, 5, 6 | final verification | none |

## Todos
> Implementation + Test = ONE todo. Never separate.
<!-- APPEND TASK BATCHES BELOW THIS LINE WITH edit/apply_patch - never rewrite the headers above. -->
- [x] 1. Safety baseline and dirty-worktree guard
  What to do / Must NOT do: Record current `git status --short`, note unrelated untracked files, and confirm no product Rust symbol edit is planned. If the worker later decides to edit Rust code, stop and run GitNexus impact analysis on the target symbol first. Must NOT modify source/docs in this todo except evidence files.
  Parallelization: Wave 1 | Blocked by: none | Blocks: 2-7
  References (executor has NO interview context - be exhaustive): `AGENTS.md` GitNexus rules; current planner evidence that untracked `.claude/`, `AGENTS.md`, `CLAUDE.md`, `.omo/run-continuation/*` exist; `.gitignore:8-11` keeps `.env` secrets untracked.
  Acceptance criteria (agent-executable): `.omo/evidence/task-1-repo-setup-scripts.md` contains `git status --short`, explicit out-of-scope dirty paths, and a statement that planned edits are limited to `setup.sh`, `setup.bat`, `README.md`, `docs/DOCKER-QUICKSTART.md`, and evidence unless a new blocker appears.
  QA scenarios (name the exact tool + invocation): Happy: run `git status --short` and save output. Failure: verify `.env` is not staged/tracked by checking `git check-ignore .env` returns ignored. Evidence `.omo/evidence/task-1-repo-setup-scripts.md`.
  Commit: N | n/a

- [x] 2. Shared setup behavior contract before script implementation
  What to do / Must NOT do: Draft a concise implementation contract in `.omo/evidence/task-2-repo-setup-scripts.md` that both scripts must follow: menu labels, prerequisites, binary mode steps, Docker mode steps, PATH persistence rules, `.env` backup/update rules, and error messages. Must NOT implement product scripts yet; this prevents divergent Bash/Batch behavior.
  Parallelization: Wave 1 | Blocked by: 1 | Blocks: 4, 7
  References (executor has NO interview context - be exhaustive): `README.md:20-37` existing first-run wizard; `README.md:345-374` local build commands; `.env.example:4-41` required/optional env keys; `docker-compose.yml:21-23` POSTGRES_PASSWORD requirement; `docker-compose.yml:53-55` REPO_PATH mount; `docker-compose.yml:95-115` engine runner; `crates/cih-engine/Cargo.toml:11-13`; `crates/cih-server/Cargo.toml:7-9`.
  Acceptance criteria (agent-executable): Evidence file includes exact top-level menu text for both scripts, exact build command, exact required `.env` keys, exact PATH target path (`<repo>/target/release`), and explicit decision that binary mode is build-only.
  QA scenarios (name the exact tool + invocation): Happy: inspect evidence file and confirm it contains `cargo build --release -p cih-server -p cih-engine`, `REPO_PATH`, `POSTGRES_PASSWORD`, `REPO_NAME`, and `target/release`. Failure: confirm the contract forbids `docker compose down -v` and privileged binary copies. Evidence `.omo/evidence/task-2-repo-setup-scripts.md`.
  Commit: N | n/a

- [x] 3. PATH and `.env` helper design with idempotency fixtures
  What to do / Must NOT do: Before writing final scripts, design the idempotent update algorithms and create temporary test fixtures under `tmp/` or `.omo/evidence/`: Bash profile block replacement, Windows user PATH duplicate detection strategy, `.env` backup/update behavior, and secret redaction in logs. Must NOT write to the real user's profile during this todo.
  Parallelization: Wave 1 | Blocked by: 1 | Blocks: 4, 5, 7
  References (executor has NO interview context - be exhaustive): `.gitignore:23-25` ignores `tmp/`; `.gitignore:8-11` ignores `.env`; `.env.example:4-12` required keys; `docker-compose.yml:21-23` compose failure on missing password; background explorer finding that repo has no PATH mutation precedent.
  Acceptance criteria (agent-executable): Evidence includes sample before/after for Bash profile with exactly one CIH-marked PATH block after two applications, sample Windows PATH string before/after with no duplicate `target\\release` entry, sample `.env` before/after with backup naming, and redacted password output.
  QA scenarios (name the exact tool + invocation): Happy: run helper snippets or a temporary script twice against temp files and assert exactly one PATH entry. Failure: run with existing `.env` and assert backup is created while unknown keys/comments are preserved. Evidence `.omo/evidence/task-3-repo-setup-scripts.md`.
  Commit: N | n/a

- [x] 4. Root `setup.sh` implementation and tests-after smoke
  What to do / Must NOT do: Add root `setup.sh` using `#!/usr/bin/env bash`, Bash 3.2-compatible syntax, script-location repo-root resolution, functions, clear errors, and interactive menu. Implement binary build-only mode, PATH persistence to detected shell profile (`~/.zshrc`, `~/.bashrc`, `~/.bash_profile`, or `~/.profile`) with CIH markers, Docker Compose mode `.env` creation/update/backup, prerequisite checks, and non-secret output. Must NOT require Bash 4 features, write target-repo `.env`, print passwords, or run destructive Docker commands.
  Parallelization: Wave 2 | Blocked by: 2, 3 | Blocks: 7
  References (executor has NO interview context - be exhaustive): `scripts/eval-enterprise-java.sh` for Bash style; `README.md:345-374` build/run references; `.env.example:4-41`; `docker-compose.yml:1-120`; `docs/DOCKER-QUICKSTART.md:14-20` current Docker setup positioning.
  Acceptance criteria (agent-executable): `setup.sh` exists at repo root, is executable or documented to run via `bash setup.sh`, passes `bash -n setup.sh`, includes menu options for binary and Docker Compose modes, includes no `down -v`, includes no `/usr/local/bin` copy, and has idempotent CIH-marked PATH block behavior tested against temp HOME/profile files.
  QA scenarios (name the exact tool + invocation): Happy: run `bash -n setup.sh`; run script test/dry-run path or isolated fixture mode to apply PATH twice and assert one entry; run temp `.env` generation path and assert `REPO_PATH`, `POSTGRES_PASSWORD`, `REPO_NAME` exist with password redacted in logs. Failure: run Docker mode with invalid `REPO_PATH` and assert non-zero clear error without writing target-repo files. Evidence `.omo/evidence/task-4-repo-setup-scripts.txt`.
  Commit: Y | `feat(setup): add macos linux setup wizard`

- [x] 5. Root `setup.bat` implementation and static/smoke verification
  What to do / Must NOT do: Add root `setup.bat` for Windows cmd.exe users with matching menu/options and behavior. Use cmd-compatible flow; invoke PowerShell only for safer user PATH persistence and hidden password input if needed. Persist user-level PATH idempotently via .NET environment APIs rather than blind `setx` truncation. Must NOT require admin privileges, write machine-level PATH, print passwords, or diverge from `setup.sh` behavior.
  Parallelization: Wave 2 | Blocked by: 3 | Blocks: 7
  References (executor has NO interview context - be exhaustive): `docs/DOCKER-QUICKSTART.md:30-40` Windows command examples; `docs/DOCKER-QUICKSTART.md:130-167` Windows Docker path quoting examples; `.env.example:4-41`; `docker-compose.yml:1-120`; background explorer finding no `.bat` precedent.
  Acceptance criteria (agent-executable): `setup.bat` exists at repo root, targets cmd.exe, has the same two top-level modes, uses quoted paths, avoids admin/machine PATH edits, avoids destructive Docker commands, and includes idempotent user PATH logic with duplicate detection.
  QA scenarios (name the exact tool + invocation): Happy: if `cmd.exe` is available, run `cmd.exe /c setup.bat --help` or equivalent non-destructive help/dry-run path; otherwise run an executable static check that asserts required labels/commands exist and forbidden commands (`down -v`, machine PATH, `/M`, privileged copy) do not. Failure: static check simulates existing PATH containing `target\\release` and asserts the PowerShell/cmd logic would not append duplicate entry. Evidence `.omo/evidence/task-5-repo-setup-scripts.txt`.
  Commit: Y | `feat(setup): add windows setup wizard`

- [x] 6. Documentation update for friendly first-run flow
  What to do / Must NOT do: Update `README.md` Quick Start so new users first run `./setup.sh` on macOS/Linux or `setup.bat` on Windows, then choose binary build or Docker Compose mode. Preserve manual setup fallback and existing advanced `cih-engine start` context without making it the first required step. Update `docs/DOCKER-QUICKSTART.md` to recommend Docker Compose mode in the scripts and keep manual Docker instructions as fallback. Include PATH revert/uninstall instructions for both platforms. Must NOT remove essential manual commands for users who cannot run scripts.
  Parallelization: Wave 2 | Blocked by: 3 | Blocks: 7
  References (executor has NO interview context - be exhaustive): `README.md:18-60` current Quick Start/manual setup; `README.md:328-374` troubleshooting/local build; `docs/DOCKER-QUICKSTART.md:14-20` current native wizard note; `.env.example:1-12` env template.
  Acceptance criteria (agent-executable): README contains `./setup.sh` and `setup.bat` before `cih-engine start` in Quick Start; docs Docker quickstart references Docker Compose mode; docs include how to remove the CIH PATH block/user PATH entry; manual fallback still includes `REPO_PATH` and `POSTGRES_PASSWORD`.
  QA scenarios (name the exact tool + invocation): Happy: run content assertions for `setup.sh`, `setup.bat`, `POSTGRES_PASSWORD`, and PATH revert text in README/docs. Failure: assert manual fallback still exists by checking for `docker compose up -d` and `.env` instructions. Evidence `.omo/evidence/task-6-repo-setup-scripts.txt`.
  Commit: Y | `docs(setup): document interactive setup scripts`

- [x] 7. Integrated verification and polish
  What to do / Must NOT do: Run the complete tests-after suite, fix any script/doc issues, and capture evidence. Verify no secrets or generated `.env` files are staged. If Docker/Rust are unavailable, verify prerequisite-failure paths and record the environment limitation. Must NOT claim Windows runtime execution unless actually run on Windows/cmd.exe.
  Parallelization: Wave 3 | Blocked by: 4, 5, 6 | Blocks: final verification
  References (executor has NO interview context - be exhaustive): All files changed by Todos 4-6; `.gitignore:8-11`; `.gitignore:23-25`; `docker-compose.yml:1-120`; README Quick Start after edits.
  Acceptance criteria (agent-executable): `bash -n setup.sh` passes; setup scripts contain no forbidden destructive commands; PATH idempotency checks pass; `.env` temp generation/backup checks pass; docs assertions pass; `git status --short` shows only intended files plus `.omo/evidence`; no `.env` or backup is staged/tracked; if Docker available, `docker compose config` succeeds against generated `.env`.
  QA scenarios (name the exact tool + invocation): Happy: run all verification commands and save logs. Failure: run invalid repo path and duplicate PATH fixtures; assert clear errors/no duplicates. Evidence `.omo/evidence/task-7-repo-setup-scripts.txt`.
  Commit: N | final verification only

## Final verification wave
> Runs in parallel after ALL todos. ALL must APPROVE. Surface results and wait for the user's explicit okay before declaring complete.
- [x] F1. Plan compliance audit
- [x] F2. Code quality review
- [x] F3. Real manual QA
- [x] F4. Scope fidelity
- F1 Plan compliance audit: independent read-only reviewer verifies every Must Have/Must NOT Have and todo acceptance criterion has evidence.
- F2 Code quality review: reviewer checks scripts for quoting, shell compatibility, error handling, idempotency, secret handling, and maintainability.
- F3 Real manual QA: agent actually runs available script paths/non-destructive smoke commands on the current OS and records any Windows-only limitation truthfully.
- F4 Scope fidelity: reviewer verifies no Rust code, CI config, secrets, or unrelated untracked files were modified unless explicitly justified with impact analysis.

## Commit strategy
- Suggested atomic commits if the user asks the worker to commit later:
  1. `feat(setup): add macos linux setup wizard` — `setup.sh` plus evidence if tracked by workflow.
  2. `feat(setup): add windows setup wizard` — `setup.bat`.
  3. `docs(setup): document interactive setup scripts` — README and Docker quickstart.
- Before any commit, worker must inspect `git status`, `git diff`, and ensure `.env`, backups, generated binaries, and unrelated untracked files are not staged.

## Success criteria
- A first-time macOS/Linux user can run `./setup.sh` or `bash setup.sh` from the repo root and choose binary build or Docker Compose setup.
- A first-time Windows cmd.exe user can run `setup.bat` from the repo root and choose the same two modes.
- Binary mode builds both release binaries and future terminals can run `cih-engine` / `cih-server` without `./target/release/` prefix.
- Docker Compose mode creates a compose-valid `.env`, starts the existing stack, and does not require users to manually discover `POSTGRES_PASSWORD` requirements.
- README and Docker quickstart make the new scripts the friendly setup path while retaining manual fallback.
- Verification evidence proves syntax, idempotency, `.env` behavior, docs positioning, and no secret/generated-file leakage.
