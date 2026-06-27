# setup-scripts planning draft

status: awaiting-approval
intent: CLEAR
classification: Standard
pending_action: write `.omo/plans/setup-scripts.md` after user approval.

## Original request

User asked to improve repo-level initial setup by adding interactive Windows and macOS/Linux scripts (for example `setup.sh` and `setup.bat`) that let a user choose either direct built-binary setup/run or Docker Compose setup/run. If the user chooses the binary path, the setup should add a PATH entry so commands do not need a long prefix.

## Components ledger

- C1: Root setup scripts — add `setup.sh` and `setup.bat` with matching interactive choices and safe prerequisite checks. Status: in scope. Evidence: `README.md:18-39`, `docker-compose.yml:1-120`, `Dockerfile:30-50`.
- C2: Binary/PATH path — build release binaries and expose `cih-engine`/`cih-server` without `./target/release/...`. Status: owner decision open for PATH persistence behavior. Evidence: `README.md:345-371`, `crates/cih-engine/Cargo.toml:11-13`, `crates/cih-server/Cargo.toml:7-9`, `.gitignore:1-3`.
- C3: Docker Compose path — preserve existing image-based setup using `docker compose` and `.env` requirements. Status: in scope. Evidence: `README.md:39-60`, `docker-compose.yml:18-23`, `docker-compose.yml:35-61`, `docker-compose.yml:95-115`.
- C4: Documentation and verification — update README/docs to point new users at scripts and validate scripts without destructive commands. Status: in scope. Evidence: `README.md:20-37`, `docs/DOCKER-QUICKSTART.md:14-20`, `.omo/plans/interactive-start.md:27-31`.

## Grounded facts

- Existing recommended setup is `cih-engine start`, but README notes it must run natively because Docker Compose needs `.env` before service startup (`README.md:20-37`).
- Existing manual Docker path requires `.env` with `REPO_PATH` and `POSTGRES_PASSWORD`, then `docker compose pull` and `docker compose up -d` (`README.md:39-60`, `docker-compose.yml:21-23`, `docker-compose.yml:53-55`).
- Local development build already builds both release binaries with `cargo build --release -p cih-server -p cih-engine` and runs `./target/release/cih-server` / `./target/release/cih-engine ...` (`README.md:345-371`).
- Release binary names are `cih-engine` and `cih-server` (`crates/cih-engine/Cargo.toml:11-13`, `crates/cih-server/Cargo.toml:7-9`).
- `target/` is ignored (`.gitignore:1-3`), so adding `target/release` to PATH points to local build artifacts and does not add generated binaries to git.
- Dirty worktree already has unrelated untracked files (`.claude/`, `AGENTS.md`, `CLAUDE.md`, `.omo/run-continuation/*.json`); implementation must not overwrite them.
- Background exploration confirmed there is exactly one existing shell script, `scripts/eval-enterprise-java.sh`, and no existing `.bat`/`.cmd`/`.ps1` setup pattern. The Bash style to reuse is `#!/usr/bin/env bash`, `set -uo pipefail`, script-location-relative repo-root resolution, functions, clear errors, and no silent global environment mutation.
- Background exploration confirmed there is no CI configuration (`.github/`, `.gitlab-ci.yml`, etc.), so script QA must be documented in the work plan rather than relying on an existing pipeline.
- Background exploration recommended `scripts/` for convention, but the user explicitly requested repo-level scripts like `setup.sh`/`setup.bat`; plan keeps root-level scripts as the product surface and may optionally factor helpers only if needed.

## Candidate approach

- Add root-level `setup.sh` and `setup.bat`.
- Both scripts start from their own repo root, present a menu: binary/direct path or Docker Compose path.
- Binary/direct path builds `cih-engine` and `cih-server`, prepends repo `target/release` to PATH for the script session, and only persists PATH after an explicit user confirmation.
- Docker path uses current `docker-compose.yml` and checks/writes the required `.env` keys before `docker compose pull/up`.
- Update `README.md` and `docs/DOCKER-QUICKSTART.md` so first-time users run `setup.sh` or `setup.bat` before the detailed manual fallback.

## Open owner decisions

- OD1: PATH persistence behavior. Answered by user: always persist. Plan must still make the script transparent by printing what will be changed before changing it.
- OD2: Binary path meaning. Answered by user: build only. Plan scope is building release binaries and configuring PATH, not launching/indexing with the local binaries.
- OD3: Test strategy. Answered by user: tests-after.

## Approved-brief candidate

Approach to plan after approval:

- Implement root `setup.sh` and `setup.bat` as first-run interactive scripts.
- Both scripts offer exactly two top-level modes: binary build mode and Docker Compose mode.
- Binary build mode runs `cargo build --release -p cih-server -p cih-engine`, verifies `target/release/cih-engine` and `target/release/cih-server` exist, and persistently adds the repo's `target/release` directory to PATH so future commands can use `cih-engine`/`cih-server` directly.
- macOS/Linux PATH persistence should update a detected user shell profile (`~/.zshrc`, `~/.bashrc`, or `~/.profile`) idempotently with a CIH-marked block. Windows PATH persistence should use user-level PATH via `setx` or PowerShell user environment APIs, idempotently avoiding duplicate entries.
- Docker Compose mode should prompt/check required `.env` keys (`REPO_PATH`, `POSTGRES_PASSWORD`, optional `REPO_NAME`), then run the existing `docker compose pull` / `docker compose up -d` path and show next commands for engine/indexing.
- Docs update should move README Quick Start to `setup.sh`/`setup.bat`, keep manual fallback, and align `docs/DOCKER-QUICKSTART.md`.
- Tests-after QA should include shell syntax checks, Windows batch parse/smoke where available, idempotency checks for PATH block generation, and non-destructive smoke runs using temporary HOME/USERPROFILE where feasible. If the worker adds script-generated temp files, they must go under already-ignored `tmp/` or `target/`.

## Test strategy candidate

Tests-after: script syntax/static validation plus non-destructive dry-run/smoke invocations, because the change is mostly setup orchestration and docs. Agent-executed QA remains mandatory.
