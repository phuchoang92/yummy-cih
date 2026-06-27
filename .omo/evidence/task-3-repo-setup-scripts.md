# Task 3: Idempotent PATH & .env Algorithms — repo-setup-scripts

**Date:** 2026-06-27
**Plan:** `repo-setup-scripts`
**Goal:** Design and verify the three idempotent configuration algorithms required by the setup wizard.

---

## 1. Bash Shell Profile PATH Idempotency

### 1.1 Algorithm Specification

```
FUNCTION update_bash_path(profile_path, repo_root):
    INPUT:  profile_path — path to shell profile (.zshrc, .bashrc, etc.)
            repo_root    — absolute path to the CIH repo root
    OUTPUT: idempotent update of profile_path

    # Step 1 — Detect which profile to use (first existing file wins)
    candidates = ["$HOME/.zshrc", "$HOME/.bashrc",
                  "$HOME/.bash_profile", "$HOME/.profile"]
    profile = first candidate that exists AND is writable
    if no candidate: create "$HOME/.bashrc" as new file

    # Step 2 — Read profile content
    content = read_lines(profile)

    # Step 3 — Build replacement block
    release_dir = repo_root + "/target/release"
    new_block = [ "# >>> CIH begin >>>",
                  "export PATH=\"" + release_dir + ":$PATH\"",
                  "# <<< CIH end <<<" ]

    # Step 4 — Idempotent replace-or-append
    if content contains markers "# >>> CIH begin >>>" and "# <<< CIH end <<<":
        # REPLACE MODE: replace everything between markers (inclusive)
        # with the new block
        result = replace_between_markers(content, new_block)
    else:
        # APPEND MODE: if last line is not empty, add a blank line first
        if last_line(content) is not empty:
            result = content + [""]
        result = result + new_block

    # Step 5 — Write back
    write_lines(profile, result)
```

### 1.2 Marker Format

CIH-marked block uses ASCII-compatible boundary comments:

```
# >>> CIH begin >>>
export PATH="/absolute/path/to/cih-repo/target/release:$PATH"
# <<< CIH end <<<
```

- Markers are **case-sensitive** and must match exactly.
- The closing marker (`# <<< CIH end <<<`) is required — a stray opening marker
  without its pair is treated as "no block exists" to avoid orphaned content.
- Both markers must appear on their own lines (no trailing content).

### 1.3 Bash 3.2 Compatibility (macOS Default)

All Bash scripts in this project MUST be compatible with Bash 3.2, which lacks:

| Feature | Bash ≥ 4.0 | Bash 3.2 Workaround |
|---|---|---|
| `[[ ]]` test operator | `[[ -f "$file" ]]` | `[ -f "$file" ]` |
| Indexed arrays | `arr=(a b c)` | Not used; process line-by-line |
| Associative arrays | `declare -A map` | Not used |
| `mapfile` / `readarray` | `mapfile -t lines < file` | `while IFS= read -r line; do ... done < file` |
| `;;&` / `;&` case fallthrough | `case $x in a) ... ;;& b) ... ;; esac` | Use `if`/`elif` chains instead |

### 1.4 Idempotency Guarantee

Running `update_bash_path` twice on the same file produces identical output:

| Run | State | Action | Result |
|---|---|---|---|
| 1st | No CIH block exists | APPEND new block | One CIH block at end |
| 2nd | CIH block exists | REPLACE block with same content | Identical file |

**Proof by construction:**
- **1st run:** Appends the block → file now has one CIH block at the end.
- **2nd run:** Detects the CIH block, replaces it with identical content → no change.
- **Edge case — interrupted first run:** If only opening marker was written
  (crash, power loss, partial write), the algorithm reads the whole file looking
  for BOTH markers before deciding "block exists." A lone opening marker without
  a closing marker is treated as "no block" → new block is appended. The orphaned
  opening marker becomes a harmless comment line. The corrective third run then
  detects both markers in the new block and performs a REPLACE, removing the orphan.

### 1.5 Test Fixtures

| File | Content |
|---|---|
| [`tmp/fixtures/bashrc-before.txt`](../../tmp/fixtures/bashrc-before.txt) | Sample `.bashrc` with aliases, exports (no CIH block) |
| [`tmp/fixtures/bashrc-after.txt`](../../tmp/fixtures/bashrc-after.txt) | Same content with CIH PATH block appended at end |

**Idempotency verification:** Applying the algorithm to `bashrc-after.txt` (which
already has a CIH block) must produce identical output. See §4 for test results.

### 1.6 Sample Before/After Diff

```diff
--- bashrc-before.txt
+++ bashrc-after.txt
@@ -26,3 +26,7 @@
 
 # Homebrew
 export PATH="/opt/homebrew/bin:/opt/homebrew/sbin:$PATH"
+
+# >>> CIH begin >>>
+export PATH="/Users/duclaidinhcao/Documents/Work/VPB/yummy/yummy-cih/target/release:$PATH"
+# <<< CIH end <<<
```

---

## 2. Windows PATH Idempotency (PowerShell)

### 2.1 Algorithm Specification

```powershell
function Update-CihPath {
    param(
        [string]$RepoRoot   # e.g., "C:\Users\alice\projects\cih"
    )

    $releasePath = Join-Path $RepoRoot "target\release"

    # Step 1 — Read current USER path (NOT machine path — user-scoped only)
    # Use [Environment]::GetEnvironmentVariable to avoid setx 1024-char truncation
    $currentUserPath = [Environment]::GetEnvironmentVariable("Path", "User") ?? ""

    # Step 2 — Split into individual entries
    $entries = $currentUserPath -split ';' |
        Where-Object { $_ -ne "" } |
        ForEach-Object { $_.Trim() }

    # Step 3 — Check case-insensitive duplicate
    $alreadyExists = $entries | Where-Object {
        $_.TrimEnd('\') -eq $releasePath.TrimEnd('\')
    }

    if ($alreadyExists) {
        Write-Host "CIH PATH entry already present — no changes needed."
        return
    }

    # Step 4 — Append if not present
    $newPath = if ($currentUserPath) {
        "$currentUserPath;$releasePath"
    } else {
        $releasePath
    }

    # Step 5 — Persist (user scope)
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")

    Write-Host "CIH PATH entry added: $releasePath"
    Write-Host "Restart your terminal for the change to take effect."
}
```

### 2.2 Key Design Decisions

| Decision | Reason |
|---|---|
| Use `[Environment]::GetEnvironmentVariable` instead of `setx` | `setx` silently truncates PATH strings longer than 1024 characters |
| User scope only (not Machine) | Admin privileges not required; user-local change is sufficient |
| Case-insensitive comparison (`-eq` in PowerShell is case-insensitive) | Windows PATH is case-insensitive; avoids duplicate with `Target\Release` vs `target\release` |
| Trim trailing backslashes before comparing | `C:\foo\` vs `C:\foo` should be treated as identical |
| Skip if already present | Single check prevents duplicates on re-run |

### 2.3 Idempotency Guarantee

Running `Update-CihPath` twice produces exactly ONE `target\release` entry:

| Run | State | Action | Result |
|---|---|---|---|
| 1st | Entry absent | Append entry | One entry in PATH |
| 2nd | Entry present | Skip (no-op) | Still one entry |

---

## 3. `.env` Backup & Upsert Algorithm

### 3.1 Algorithm Specification

```
FUNCTION update_env_file(env_path, updates: dict):
    INPUT:  env_path — path to .env file (or path where it should be created)
            updates  — map of KEY → new_value for keys to upsert
    OUTPUT: idempotent update of env_path; backup created if file existed

    UPSERT_KEYS = {"REPO_PATH", "POSTGRES_PASSWORD", "REPO_NAME"}

    # Step 1 — Backup existing .env (atomic copy)
    if env_path exists:
        timestamp = format_compact(now())  # e.g., "20260627120000"
        backup_path = dirname(env_path) + "/.env.cih-backup-" + timestamp
        copy(env_path, backup_path)

    # Step 2 — Read existing lines (or empty list if new file)
    lines = read_lines(env_path) if env_path exists else []

    # Step 3 — Process line by line
    seen_keys = {}
    result = []
    for each line in lines:
        trimmed = line with leading/trailing whitespace removed

        # Preserve comments and blank lines verbatim
        if trimmed starts with "#" or trimmed is empty:
            append line to result
            continue

        # Parse KEY=VALUE (handle equals sign in value)
        if line matches /^([A-Za-z_][A-Za-z0-9_]*)="?(.*?)"?$/:
            key = match group 1
            value = match group 2 (stripped of optional quotes)

            if key in UPSERT_KEYS and key in updates:
                # REPLACE this line with the new value
                new_value = updates[key]
                append key + "=" + new_value to result
                seen_keys[key] = true
            else:
                # PRESERVE unknown key (FOO=bar, etc.)
                append line to result
        else:
            # Preserve malformed lines verbatim
            append line to result

    # Step 4 — Append any required keys not seen
    for each key in UPSERT_KEYS:
        if key in updates and key not in seen_keys:
            append key + "=" + updates[key] to result

    # Step 5 — Write back
    write_lines(env_path, result)
```

### 3.2 Secret Redaction Rule

After reading `POSTGRES_PASSWORD` from user input, all subsequent output/log
messages MUST redact the password value:

```
# CORRECT (redacted output):
echo "POSTGRES_PASSWORD=********"
echo "Password saved to .env"

# WRONG (leaks password):
echo "POSTGRES_PASSWORD=$password"
printf "Writing POSTGRES_PASSWORD=%s\n" "$password"
```

**Redaction applies to:**
- Terminal/stdout echo statements in setup wizard
- Log files (if any)
- `.env.example` templates (use `changeme` placeholder)
- Error messages (never echo the real password in "failed to validate: $pw")
- **Test fixtures** — use `changeme` or `***REDACTED***` placeholders

**Redaction does NOT apply to:** the actual `.env` file itself (that file is
excluded from git and is the canonical password store).

### 3.3 Idempotency Guarantee

Running `update_env_file` twice with the same `updates` map produces identical output:

| Run | State | Action | Result |
|---|---|---|---|
| 1st | `.env` exists with old values | Backup created; REPO_PATH/POSTGRES_PASSWORD/REPO_NAME lines replaced or appended | New values; FOO/BAR preserved; backup created |
| 2nd | `.env` has target values | Backup created again (different timestamp); REPLACE finds same values → writes same values | Identical content; second backup created |

### 3.4 Test Fixtures

| File | Description |
|---|---|
| [`tmp/fixtures/env-before.txt`](../../tmp/fixtures/env-before.txt) | Sample `.env` with old values, `FOO=bar`, `BAZ=qux`, comments, blank lines |
| [`tmp/fixtures/env-after.txt`](../../tmp/fixtures/env-after.txt) | After update: `REPO_PATH`, `POSTGRES_PASSWORD`, `REPO_NAME` replaced; `FOO=bar` & `BAZ=qux` preserved; all comments and blank lines intact |

### 3.5 Preservation Rules Summary

| Line Type | Action | Example |
|---|---|---|
| Comment (`#...`) | Preserved verbatim | `# OLD_REPO_PATH — this was used before migration` |
| Blank line | Preserved verbatim | (empty line between sections) |
| Known key (`REPO_PATH=`, `POSTGRES_PASSWORD=`, `REPO_NAME=`) | **Replaced** with new value | `REPO_PATH=/new/path` |
| Unknown key (`FOO=bar`, `CUSTOM_KEY=...`) | Preserved verbatim | `FOO=bar` remains unchanged |
| Malformed line (no `=`) | Preserved verbatim | (any unrecognized format) |
| Required key not present | **Appended** at end | `REPO_NAME=new-repo-name` appended if missing |

### 3.6 Backup Naming Convention

```
.env.cih-backup-YYYYMMDDHHmmss
```

- `YYYYMMDDHHmmss` = UTC timestamp of the backup creation moment
- Example: `.env.cih-backup-20260627143021`
- Multiple runs = multiple backups (different timestamps, never overwrite)
- Backups are gitignored by `.gitignore:10` (`.env.*` pattern)

---

## 4. Bash Idempotency Test

To verify the Bash PATH algorithm is truly idempotent, we simulate two
consecutive runs on a file that already has the CIH block.

### 4.1 Test Script

The test uses `sed` to implement the replace-or-append logic inline
(simulating what the real `setup.sh` would do) and checks that applying
it twice produces identical output.

### 4.2 Test Results (2026-06-27)

All tests executed on macOS with Bash 3.2:

```
=== CIH PATH Idempotency Test ===

--- Test 1: Idempotency on file with existing CIH block ---
  [PASS] Run 1 == Run 2 (idempotent)
  [PASS] Exactly one CIH block (count=1)

--- Test 2: Fresh injection on file without CIH block ---
  [PASS] Before file has no CIH markers
  [PASS] Append then replace = idempotent
  [PASS] Exactly one CIH block (count=1)

--- Test 3: Orphaned opening marker recovery ---
  [PASS] Only opening marker -> APPEND mode
  [PASS] Orphan recovery stabilizes (idempotent)

--- Test 4: Windows PATH idempotency (simulated) ---
  Initial PATH: C:\Users\alice\...\Scripts;C:\Windows\system32;C:\Program Files\Git\cmd
  Run 1 PATH:   ...Scripts;C:\Windows\system32;C:\Program Files\Git\cmd;C:\Users\alice\projects\cih\target\release
  Run 2 PATH:   ...Scripts;C:\Windows\system32;C:\Program Files\Git\cmd;C:\Users\alice\projects\cih\target\release
  [PASS] Windows PATH idempotent — Run 1 == Run 2
  [PASS] Exactly one target\release entry (count=1)
  [PASS] Case-insensitive match — no duplicate

=== ALL 4 TESTS PASSED ===
```

**Verification:**
- The diff between first application output and second application output
  is **empty** — confirming that a file with the CIH block is left
  unchanged when the algorithm runs again.
- Windows PATH simulation confirmed exactly one `target\release` entry
  after two applications and correct case-insensitive duplicate detection.
- Orphaned marker recovery stabilizes to idempotency after 2 runs.

### 4.3 Key Implementation Detail: Trailing Blank-Line Normalization

The critical insight discovered during testing: after `sed` deletes the CIH
block, the file may retain a trailing blank line from the separator that
existed before the block. If the algorithm then appends another blank line
before the new block, a cycle of `n → n+1` blank lines accumulates.

**Fix:** After deleting the CIH block, strip ALL trailing blank lines, then
append exactly one blank line + the block. This guarantees idempotency:

```bash
# Strip trailing blank lines after deleting CIH block
while [ -s "$file" ] && [ "$(tail -1 "$file")" = "" ]; do
    total=$(wc -l < "$file" | tr -d ' ')
    sed -i '' -e "${total}d" "$file"
done
```

---

## 5. Design Constraints & Rationale

| Constraint | Source | Rationale |
|---|---|---|
| Bash 3.2 compatible | macOS ships Bash 3.2 as default | `setup.sh` must work on macOS without user installing Bash 4+ |
| No `[[ ]]` | Bash 3.2 limitation | Use `[ ]` with proper quoting |
| No arrays | Bash 3.2 limitation | Process files line-by-line with `while read` |
| CIH markers are ASCII comments | Cross-shell compatibility | Works in `zsh`, `bash`, `sh`, `fish` (fish ignores unknown comments) |
| `.env.cih-backup-*` naming | Uniqueness + discoverability | Timestamp ensures no overwrite; `cih-` prefix groups related backups |
| PowerShell user scope | Windows security | Avoids UAC elevation prompts; user-scoped PATH is sufficient |
| `[Environment]::SetEnvironmentVariable` | Windows reliability | Avoids `setx` 1024-char truncation bug that silently corrupts long PATH strings |
| Redact `POSTGRES_PASSWORD` in logs | Security | Password must never appear in terminal output, logs, or error messages |
