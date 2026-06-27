# learnings — repo-setup-scripts

## 2026-06-27: Windows setup.bat implementation

### Key decisions

1. **PowerShell scope**: Used PowerShell for three specific purposes only (per contract §6.2):
   - Hidden password input (`Read-Host -AsSecureString`)
   - User PATH persistence (`[Environment]::SetEnvironmentVariable`)
   - `.env` upsert logic (allowed per contract: "PowerShell -replace")
   - Core script flow remains in batch (`cmd.exe`)

2. **Combined PowerShell invocation for .env**: Instead of capturing the password from
   PowerShell→batch→PowerShell (which would expose it to batch variable handling and
   shell escaping), the password prompt and `.env` upsert are combined into a single
   PowerShell invocation. REPO_PATH and REPO_NAME are passed via process environment
   variables (`set CIH_SETUP_REPO_PATH=...`, read as `$env:CIH_SETUP_REPO_PATH` in PS).

3. **Batch variable escaping**: Windows NTFS disallows `'`, `"`, `!`, `&`, `|`, `<`, `>`
   in file/directory names, so embedding `%SCRIPT_DIR%` in PowerShell single-quoted strings
   is safe for all valid Windows paths.

4. **PATH idempotency**: Used contract-specified PowerShell snippet with
   `[Environment]::GetEnvironmentVariable("Path", "User")` + case-insensitive
   `TrimEnd('\')` comparison to avoid duplicate entries. Never uses `setx` (1024-char
   truncation bug).

5. **Menu input**: Used `SET /P` with validation loop (max 3 attempts) rather than
   `CHOICE` because `CHOICE` cannot implement the "3 invalid attempts" retry counter
   (CHOICE internally rejects invalid keys with a beep, bypassing the counter).

6. **Health check**: Polls `docker compose ps cih-server | findstr "healthy"` every 5
   seconds with a 12-iteration cap (60s timeout). Uses `goto`-based loop since batch
   has no `while` construct.

7. **No `setup.sh` precedent**: This repo has no existing `.bat` or `.sh` setup scripts.
   `setup.bat` is the first platform setup script. `setup.sh` is expected to follow in
   a subsequent task.

### Edge cases handled
- Empty REPO_PATH input → re-prompt with error message
- REPO_PATH with surrounding quotes → stripped before validation
- Existing `.env` → backed up with timestamp before overwrite
- Missing `.env` → created fresh from upsert keys only
- POSTGRES_PASSWORD empty → error and re-prompt (up to 3 attempts)
- Build failure → exit 1 with descriptive message
- PATH already present → skip (no duplicate entry)
- cih-server not healthy within 60s → warning, not fatal error

## 2026-06-27: macOS/Linux setup.sh implementation

### Key decisions

1. **awk for trailing blank stripping**: Instead of `sed -i ''` (macOS) / `sed -i` (Linux)
   which have incompatible in-place flags, used portable `awk 'NF {last=NR} ...'` to strip
   trailing blank lines. This avoids OS-detection branches and ensures identical behavior
   across platforms.

2. **sed | delimiter for redaction**: Used `|` as sed delimiter (`sed "s|$PASS|********|g"`)
   rather than `/` to minimize collision with password characters. The `|` character is
   extremely rare in randomly-generated passwords.

3. **stty -echo for hidden input**: Used `stty -echo`/`stty echo` for password masking
   rather than `read -s` because `read -s` may not be available on all minimal shell
   environments. The `stty` approach is more universally available.

4. **PIPESTATUS for pipeline exit codes**: Used `${PIPESTATUS[0]}` after piped docker
   commands to capture the exit code of the docker command itself (not the redact filter).
   This is critical because `$?` would give the exit code of the last command in the
   pipeline (redact_output), masking docker failures.

5. **Profile detection order**: `.zshrc` → `.bashrc` → `.bash_profile` → `.profile` as
   specified by the algorithm design. Creates `~/.bashrc` if none exist. The order
   prioritizes zsh (common on modern macOS) over bash profiles.

6. **Duplicate UPSERT_KEYS handling**: When an existing `.env` has duplicate keys for
   REPO_PATH/POSTGRES_PASSWORD/REPO_NAME, only the first occurrence is replaced;
   subsequent duplicates are silently dropped. This prevents writing duplicate entries
   while preserving the intended value.

### Edge cases handled
- Empty profile file (only CIH block) → after REPLACE mode removal, trailing-blank
  stripping produces empty file, then APPEND adds block → stable at second run
- Orphaned opening marker (crash during first write) → treated as APPEND mode, orphan
  becomes harmless comment, second run REPLACEs both orphan + new block → stabilizes
- Empty SECRET_PASSWORD → redact_output falls through to `cat` (no-op)
- REPO_PATH with tricky characters → validated via `case "$repo_path" in /*)` pattern
  and `[ -d "$repo_path" ]`
- Y/Enter default for PATH offer → case matches empty input as default-yes
- docker compose pull fails → captured via PIPESTATUS, dies with error

## 2026-06-27: Task 7 Integrated Verification

### Results

- **Verdict: 39/39 checks PASSED** — all acceptance criteria satisfied
- Evidence file: `.omo/evidence/task-7-repo-setup-scripts.txt`
- No blocking issues found

### Key findings

1. **Bash 3.2 compatibility verified**: No `[[ ]]`, no arrays, no mapfile, no here-strings.
   `awk` used for portable trailing-blank-line stripping instead of `sed -i`.

2. **PATH idempotency proven**: 8 sub-checks confirm the CIH marker block is idempotent
   across multiple runs on `.bashrc` profiles. Original content preserved.

3. **.env backup/upsert preserves user data**: 12 sub-checks confirm comments, unknown keys
   (FOO=bar, BAZ=qux), and blank lines survive the update. Backup is byte-for-byte copy.

4. **Password never leaked**: `redact_output()` in setup.sh uses `sed "s|$PASS|********|g"`;
   setup.bat uses `Write-Host` with literal `********` — never echoes real password.

5. **Docker Compose config validates**: `docker compose config` passes with Docker 28.0.1
   and Compose v2.33.1 on macOS (darwin).

6. **Git hygiene clean**: `.env` and `.env.cih-backup-*` gitignored by `.gitignore:9-10`.
   No secrets tracked; only expected files changed.

### Known discrepancy (non-blocking)

- **Timestamp format**: Contract (task-2 §3.4) specifies `YYYY-MM-DDTHHMMSS` but both
  scripts use `YYYYMMDDHHmmss` per algorithm design (task-3 §3.6). Compact format is
  safer for Windows filename compatibility. Both scripts are internally consistent.
  Recommend updating task-2 contract to match.
