# Task 2: Shared Setup Behavior Contract — `setup.sh` / `setup.bat`

**Date:** 2026-06-27
**Plan:** `repo-setup-scripts`
**Goal:** Define a single contract both `setup.sh` and `setup.bat` must follow,
preventing divergent Bash/Batch behavior.

---

## Contract Scope

This document is the AUTHORITATIVE specification for both scripts. Any deviation
between `setup.sh` and `setup.bat` is a bug. Both scripts must implement the same
user-facing flow, the same prereq checks, the same `.env` handling, and the same
error messages.

---

## 1. Top-Level Menu

Both scripts MUST render exactly this menu, byte-for-byte identical labels:

```
══ CIH Setup ══
1) Binary build (cargo build --release)
2) Docker Compose setup
q) Quit
```

- The header uses `══` (U+2550 double horizontal box-drawing) on both sides.
- Choice 1 enters **binary mode** (build only, see §4).
- Choice 2 enters **Docker mode** (Compose-based, see §5).
- `q` or `Q` exits with code 0.
- Any other input re-prompts. Maximum 3 invalid attempts, then exit with code 1 and
  message `"Too many invalid choices."`

---

## 2. Prerequisite Checks

Both scripts MUST check prerequisites BEFORE showing the menu (fail-fast).

### 2.1. Cargo check (for binary mode)

| Script | Command | Error if missing |
|--------|---------|------------------|
| `setup.sh` | `command -v cargo` | `"ERROR: Rust/Cargo not found. Install from https://rustup.rs and try again."` |
| `setup.bat` | `where cargo` | Same message. |

Only required when the user picks option 1 (binary mode). However, both scripts
SHOULD check at menu-entry time to give early feedback.

### 2.2. Docker check (for Docker mode)

| Script | Command | Error if missing |
|--------|---------|------------------|
| `setup.sh` | `command -v docker` | `"ERROR: Docker not found. Install Docker Desktop from https://docker.com and try again."` |
| `setup.bat` | `where docker` | Same message. |

Only required when the user picks option 2. Docker Compose (v2) is also required;
check with `docker compose version`. Error: `"ERROR: Docker Compose v2 not found. Update Docker Desktop and try again."`

### 2.3. Directory context

Both MUST verify they are running from the repo root (where `docker-compose.yml`,
`Cargo.toml`, and `.env.example` live). Detection:

| Script | Method |
|--------|--------|
| `setup.sh` | `SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"` — then verify `"$SCRIPT_DIR/docker-compose.yml"` exists. |
| `setup.bat` | `%~dp0` then check `%SCRIPT_DIR%\docker-compose.yml` exists. |

Error if absent: `"ERROR: docker-compose.yml not found. Run this script from the yummy-cih repo root."`

---

## 3. `.env` Handling

### 3.1. Required keys

The `.env` file must contain exactly these keys. Others are optional/preserved.

| Key | Required | Source | Default |
|-----|----------|--------|---------|
| `REPO_PATH` | Yes | User input (absolute path) | — |
| `POSTGRES_PASSWORD` | Yes | User input (hidden) | — |
| `REPO_NAME` | No | User input | `repo` |

Evidence references:
- `README.md:43-51` documents REPO_PATH and POSTGRES_PASSWORD as required.
- `.env.example:7` defines `REPO_PATH=/path/to/your/repo`.
- `.env.example:11` defines `POSTGRES_PASSWORD=changeme`.
- `.env.example:38` defines `# REPO_NAME=my-repo` (commented, optional).
- `docker-compose.yml:22` fails fatally if POSTGRES_PASSWORD is unset:
  `${POSTGRES_PASSWORD:?POSTGRES_PASSWORD must be set in .env}`.

### 3.2. REPO_PATH validation

- MUST be an absolute path.
- MUST exist on disk (the directory must be readable).
- MUST print error: `"ERROR: REPO_PATH does not exist: <path>"` followed by
  `"REPO_PATH must be an absolute path to a Java/Spring repository."`
- Re-prompt up to 3 times, then exit with code 1.

### 3.3. POSTGRES_PASSWORD validation

- MUST be non-empty.
- Input MUST be hidden (no echo).
- Error: `"ERROR: POSTGRES_PASSWORD cannot be empty."`
- Re-prompt up to 3 times, then exit with code 1.

### 3.4. Backup rule

If `.env` already exists in `SCRIPT_DIR`:

1. Create backup: `.env.cih-backup-<timestamp>`
   - Timestamp format: `YYYY-MM-DDTHHMMSS` (ISO-ish, no colons for Windows safety).
   - Example: `.env.cih-backup-2026-06-27T143022`
2. The backup MUST be a byte-for-byte copy of the old `.env`.
3. Write the new `.env` preserving:
   - All comment lines (starting with `#`)
   - All unknown/unrecognized key=value lines
   - Only overwrite values for `REPO_PATH`, `POSTGRES_PASSWORD`, `REPO_NAME`
4. The merged result is written to `.env`.

Implementation notes:
- Bash: parse existing `.env` line by line. For lines matching `^REPO_PATH=`,
  `^POSTGRES_PASSWORD=`, `^REPO_NAME=`, replace the value; keep everything else as-is.
- Batch: same logic with `for /f` tokens or PowerShell `-replace`.

### 3.5. Write location

`.env` is written to `SCRIPT_DIR` (the repo root), NOT inside the target repository
at `REPO_PATH`. Evidence: `.gitignore:8-11` ignores `.env` at the repo root;
`docker-compose.yml` reads `.env` from its own directory.

---

## 4. Binary Mode (Option 1)

### 4.1. Behavior

Build-only. The script MUST NOT run or index anything after the build.

### 4.2. Build command

```
cargo build --release -p cih-server -p cih-engine
```

Evidence: `README.md:352`.

### 4.3. Build failure

If `cargo build` returns a non-zero exit code:
```
ERROR: cargo build failed. See output above for details.
```
Exit with code 1.

### 4.4. Success message

After a successful build:
```
Binary built at <SCRIPT_DIR>/target/release/

NOTE: Docker dependencies (FalkorDB, Postgres) are still required at runtime.
Run Docker mode (option 2) or:
  docker compose up -d falkordb postgres
```

### 4.5. PATH

The script MUST offer to add `<SCRIPT_DIR>/target/release` to the user's PATH.

| Platform | Persistence method | Command |
|----------|-------------------|---------|
| Bash | Append to `~/.bashrc` (or `~/.zshrc` if `$SHELL` contains `zsh`) | `export PATH="$PATH:<SCRIPT_DIR>/target/release"` |
| Batch | PowerShell user PATH | `[Environment]::SetEnvironmentVariable("Path", ..., "User")` |

**Rules:**
- MUST NOT use `setx /M` (system-level PATH). Use user-level only.
- MUST NOT copy binary to `/usr/local/bin` or `C:\Windows`.
- On Windows, use PowerShell **only** for PATH persistence (not for the entire script).
- Show a message: `"Added target/release to your PATH. Restart your terminal for changes to take effect."`
- Offer a skip option (default to yes).

---

## 5. Docker Mode (Option 2)

### 5.1. Prerequisites

- Docker must be installed (checked in §2.2).
- `.env` must exist (written in §3).

### 5.2. Steps (in order)

1. `docker compose pull` — pulls the images.
2. `docker compose up -d` — starts FalkorDB, Postgres, and cih-server.

Evidence: `README.md:57-59`.

### 5.3. Health wait

After `up -d`, wait for `cih-server` to become healthy (poll `docker compose ps`).
Timeout after 60 seconds.

- Success: `"CIH is ready at http://localhost:8080/mcp"`
- Timeout: `"WARNING: cih-server did not become healthy within 60s. Check docker compose logs cih-server"`

### 5.4. Forbidden operations

Both scripts MUST NOT execute:
- `docker compose down -v` (destroys FalkorDB + pgvector data volumes).
- `docker compose down` without explicit user confirmation.

---

## 6. Platform-Specific Constraints

### 6.1. Bash (`setup.sh`)

- Shebang: `#!/usr/bin/env bash`
- **Must be Bash 3.2 compatible.** That means:
  - No indexed arrays (`declare -a`, `arr=(...)`, `${arr[i]}`).
  - No `mapfile` / `readarray`.
  - No associative arrays (`declare -A`).
  - No `[[ ... ]]` — use `[ ... ]` instead.
  - No `<<<` here-strings — use `printf` or here-docs.
  - No `**` globstar.
  - Use `printf` instead of `echo -e` for portability.
- Sets `set -uo pipefail` (as in `scripts/eval-enterprise-java.sh:18`).
- Uses `BASH_SOURCE`-relative pathing (`"$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"`).
  Evidence: `scripts/eval-enterprise-java.sh:20`.

### 6.2. Batch (`setup.bat`)

- Target: `cmd.exe` (the legacy Windows shell).
- MUST NOT depend on PowerShell for the main script flow.
- PowerShell allowed ONLY for:
  - Hidden password input (`Read-Host -AsSecureString`).
  - User PATH persistence (`[Environment]::SetEnvironmentVariable`).
- Use `where` for command detection.
- Use `%ERRORLEVEL%` for exit code checks.
- Use `set /p` for user prompts.
- Timestamps: `%DATE:/=-%_%TIME::=-%` (sanitized for filenames).
- Path separators: backslash (`\`).
- `.env` parsing: `for /f "tokens=1,* delims==" %%a in (.env) do ...`

---

## 7. Error Messages (Complete Table)

| Condition | Bash message | Batch message |
|-----------|-------------|---------------|
| Cargo not found | `"ERROR: Rust/Cargo not found. Install from https://rustup.rs and try again."` | Same |
| Docker not found | `"ERROR: Docker not found. Install Docker Desktop from https://docker.com and try again."` | Same |
| Docker Compose v2 not found | `"ERROR: Docker Compose v2 not found. Update Docker Desktop and try again."` | Same |
| docker-compose.yml missing | `"ERROR: docker-compose.yml not found. Run this script from the yummy-cih repo root."` | Same |
| Invalid REPO_PATH | `"ERROR: REPO_PATH does not exist: <path>\nREPO_PATH must be an absolute path to a Java/Spring repository."` | Same |
| Empty POSTGRES_PASSWORD | `"ERROR: POSTGRES_PASSWORD cannot be empty."` | Same |
| Build failure | `"ERROR: cargo build failed. See output above for details."` | Same |
| Too many invalid menu choices | `"Too many invalid choices."` | Same |
| Not running from repo root | `"ERROR: Cargo.toml not found. Run this script from the yummy-cih repo root."` | Same |

---

## 8. Forbidden Operations (Both Platforms)

| Operation | Reason |
|-----------|--------|
| `docker compose down -v` | Destroys FalkorDB + pgvector data volumes (`docker-compose.yml:117-120`). |
| `setx /M` (Windows) or `export PATH` in `/etc/profile` (Unix) | Modifies system-level PATH. User-level only. |
| Copy binary to `/usr/local/bin` or `C:\Windows` | Pollutes system directories. Binary stays in `target/release/`. |
| Write `.env` into `REPO_PATH` | `.env` must live next to `docker-compose.yml` (repo root). Writing into target repo would require repo-scoped `.gitignore` entries. |
| `rm -rf` or recursive delete without confirmation | Safety. |
| Modify product Rust source files | Out of scope per Task 1 boundaries. |

---

## 9. Evidence References (with Line Numbers)

| File | Lines | What it proves |
|------|-------|----------------|
| `README.md:37` | 37 | Docker Compose cannot run before `.env` exists. |
| `README.md:43-51` | 43-51 | Manual `.env` keys: `REPO_PATH`, `POSTGRES_PASSWORD`, `REPO_NAME`. |
| `README.md:57-59` | 57-59 | Docker Compose commands: `pull`, `up -d`. |
| `README.md:345-352` | 345-352 | Build command: `cargo build --release -p cih-server -p cih-engine`. |
| `README.md:354-358` | 354-358 | Binary path: `./target/release/cih-server`. |
| `.env.example:7` | 7 | `REPO_PATH` key definition. |
| `.env.example:11` | 11 | `POSTGRES_PASSWORD` key definition. |
| `.env.example:38` | 38 | `REPO_NAME` optional key definition. |
| `.env.example:1-2` | 1-2 | Comment lines that must be preserved. |
| `docker-compose.yml:22` | 22 | `${POSTGRES_PASSWORD:?...}` — fatal if unset. |
| `docker-compose.yml:55` | 55 | `cih-server` mounts `${REPO_PATH}:/repo:ro`. |
| `docker-compose.yml:108-109` | 108-109 | `engine` service mounts `${REPO_PATH}:/repo`. |
| `docker-compose.yml:90` | 90 | `docs-viewer` mounts `${REPO_PATH}/.cih/wiki/pages`. |
| `docker-compose.yml:117-120` | 117-120 | Volumes that `down -v` would destroy. |
| `.gitignore:8-11` | 8-11 | `.env` and `.env.*` gitignored; `.env.example` NOT ignored. |
| `scripts/eval-enterprise-java.sh:18` | 18 | Precedent: `set -uo pipefail`. |
| `scripts/eval-enterprise-java.sh:20` | 20 | Precedent: `BASH_SOURCE`-relative pathing pattern. |
| `docs/DOCKER-QUICKSTART.md:16-18` | 16-18 | Wizard must run natively (binary) before Docker. |

---

## 10. Verification

- [ ] Menu text matches §1 exactly in both scripts.
- [ ] Prerequisite checks match §2 in both scripts.
- [ ] `.env` backup/merge matches §3 in both scripts.
- [ ] Binary mode build-only behavior matches §4.
- [ ] Docker mode commands match §5.
- [ ] Bash script is 3.2 compatible (no arrays, no mapfile, no `[[`, no `<<<`).
- [ ] Batch script targets cmd.exe; PowerShell only for password + PATH.
- [ ] Error messages match §7 word-for-word.
- [ ] No forbidden operations from §8 appear in either script.
- [ ] All evidence references in §9 are accurate.
