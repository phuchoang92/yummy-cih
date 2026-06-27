---
slug: repo-setup-scripts
status: drafting
intent: clear
pending-action: write .omo/plans/repo-setup-scripts.md
approach: Add root setup.sh/setup.bat interactive scripts with two modes: build-only local binaries plus persistent PATH, or Docker Compose bootstrap. Update docs and verify with tests-after QA.
---

# Draft: repo-setup-scripts

## Components (topology ledger)
| id | outcome | status | evidence path |
| --- | --- | --- | --- |
| C1 | Root setup scripts provide interactive first-run entrypoints for macOS/Linux and Windows | active | README.md:18-39; scripts/eval-enterprise-java.sh style from background explorer |
| C2 | Binary mode builds release binaries and persistently shortens command prefix via PATH | active | README.md:345-371; crates/cih-engine/Cargo.toml:11-13; crates/cih-server/Cargo.toml:7-9; .gitignore:1-3 |
| C3 | Docker Compose mode writes/checks .env and starts the existing compose stack | active | docker-compose.yml:18-23,35-61,95-115; .env.example:4-41 |
| C4 | README and Docker quickstart make the new scripts the friendly repo-level entrypoint | active | README.md:18-39; docs/DOCKER-QUICKSTART.md:14-20 |
| C5 | Tests-after QA proves script syntax, idempotency, docs positioning, and non-destructive behavior | active | no CI per background explorer; existing cargo/npm QA in README.md:345-374 |

## Open assumptions (announced defaults)
| assumption | adopted default | rationale | reversible? |
| --- | --- | --- | --- |
| Windows script shell | `setup.bat` targets cmd.exe and may invoke PowerShell internally for safe user PATH/password operations | User explicitly requested `.bat`; docs already show Windows command examples | yes |
| Bash compatibility | `setup.sh` stays compatible with macOS stock Bash 3.2; no Bash 4 arrays/mapfile | Supports macOS/Linux without requiring Homebrew bash | yes |
| Binary mode runtime | Binary mode is build-only, not run/index; it prints next-step notes for runtime DB dependencies | User selected "Build only" | yes |
| PATH persistence | Always persist idempotently, but print exactly what changed and how to revert | User selected "Always persist"; transparency reduces risk | yes |
| REPO_NAME | Docker mode defaults to sanitized target repo folder name; optional override | Matches existing wizard's user-friendly behavior | yes |

## Findings (cited - path:lines)
- README currently points first-time users to `cih-engine start`, which assumes a native/prebuilt binary already exists (`README.md:20-37`).
- Manual setup requires `.env` beside `docker-compose.yml` with `REPO_PATH` and `POSTGRES_PASSWORD` (`README.md:39-51`; `.env.example:4-12`).
- Compose fails fast without `POSTGRES_PASSWORD` (`docker-compose.yml:21-23`) and mounts `REPO_PATH` into server/engine (`docker-compose.yml:53-55`, `docker-compose.yml:108-110`).
- Release binaries are `cih-engine` and `cih-server` (`crates/cih-engine/Cargo.toml:11-13`; `crates/cih-server/Cargo.toml:7-9`) and README's local build command is `cargo build --release -p cih-server -p cih-engine` (`README.md:345-374`).
- `target/` is ignored, so putting `target/release` on PATH exposes local build outputs without tracking binaries (`.gitignore:1-3`).
- There is no existing Windows script precedent; Bash precedent is one script with portable shebang/strict-mode/function style (background explorer; `scripts/eval-enterprise-java.sh`).
- No CI exists, so verification must be explicitly executable by the worker rather than delegated to a pipeline (background explorer).

## Decisions (with rationale)
- Add root-level `setup.sh` and `setup.bat` because the user asked for repo-level scripts like those names; do not hide the entrypoint under `scripts/`.
- Binary mode will only build both release binaries, smoke-check them, persist `target/release` to PATH, and print next steps. It will not start FalkorDB/Postgres or run indexing because the user selected "Build only".
- Docker Compose mode will create/update `.env` using required keys (`REPO_PATH`, `POSTGRES_PASSWORD`) and optional `REPO_NAME`, back up an existing `.env`, validate with `docker compose config`, then run `docker compose pull` and `docker compose up -d`.
- Avoid Rust code changes; the scripts can produce the required `.env` superset themselves. Docs should promote scripts over the existing `cih-engine start` path to avoid first-run confusion.
- Tests-after: implement scripts/docs first, then run syntax/static checks, PATH idempotency in temporary HOME/USERPROFILE, `.env` generation checks, and docs checks.

## Scope IN
- `setup.sh` at repository root.
- `setup.bat` at repository root.
- README Quick Start update to make scripts the first-time path.
- `docs/DOCKER-QUICKSTART.md` update to point Docker users at the Docker mode while keeping manual fallback.
- Non-destructive verification evidence under `.omo/evidence/`.

## Scope OUT (Must NOT have)
- No CI pipeline additions.
- No copying binaries to `/usr/local/bin`, `C:\Windows`, or other privileged/global locations.
- No destructive Docker commands such as `docker compose down -v`.
- No changes to Rust product code unless implementation discovers a hard blocker and records impact analysis first.
- No editing unrelated untracked files (`.claude/`, `AGENTS.md`, `CLAUDE.md`, `.omo/run-continuation/*`).
- No committing secrets or generated `.env` files.

## Open questions
None. User approved decisions: binary mode = build only; PATH = always persist; test strategy = tests-after.

## Approval gate
status: approved
approved-by-user: yes
pending-action: `.omo/plans/repo-setup-scripts.md` generated
