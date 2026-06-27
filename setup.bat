@echo off
setlocal enabledelayedexpansion

rem ─────────────────────────────────────────────────────────────────────────────
rem  CIH Setup Wizard — Windows (cmd.exe)
rem  Interactive setup: binary build or Docker Compose mode.
rem  Contract: .omo/evidence/task-2-repo-setup-scripts.md
rem
rem  Output is rendered as a "timeline": each checkpoint is a dot (●) joined by
rem  connector lines (│). Successful checkpoints show ✓, failed ones ✗.
rem  ANSI color escape codes are emitted when stdout is a console and NO_COLOR is
rem  not set; otherwise all glyph/color upgrades silently degrade to plain text.
rem  No PowerShell is used for the wizard flow itself — only for hidden password
rem  input and PATH persistence (per contract §6.2).
rem ─────────────────────────────────────────────────────────────────────────────

rem ── Resolve script directory ───────────────────────────────────────────────
set "SCRIPT_DIR=%~dp0"
cd /d "%SCRIPT_DIR%" 2>nul
if %ERRORLEVEL% neq 0 (
    echo ERROR: Cannot change to script directory: %SCRIPT_DIR%
    exit /b 1
)

rem ── Verify repo root context ───────────────────────────────────────────────
if not exist "%SCRIPT_DIR%docker-compose.yml" (
    echo ERROR: docker-compose.yml not found. Run this script from the yummy-cih repo root.
    exit /b 1
)
if not exist "%SCRIPT_DIR%Cargo.toml" (
    echo ERROR: Cargo.toml not found. Run this script from the yummy-cih repo root.
    exit /b 1
)

rem ── Capture ESC char for ANSI colors (pure cmd, no PowerShell) ─────────────
rem  `prompt $E` sets the cmd prompt to a literal ESC; piping to a fresh cmd
rem  echoes that prompt, which is captured into %ESC% by the for /f loop.
if defined NO_COLOR (
    set "ESC="
) else (
    for /f "delims=" %%E in ('prompt $E^|cmd') do set "ESC=%%E"
)
if not defined ESC set "ESC="

set "C_RESET=!ESC![0m"
set "C_BOLD=!ESC![1m"
set "C_DIM=!ESC![2m"
set "C_RED=!ESC![31m"
set "C_GREEN=!ESC![32m"
set "C_YELLOW=!ESC![33m"
set "C_CYAN=!ESC![36m"

set "DOT_RUN=!C_CYAN!●!C_RESET!"
set "DOT_OK=!C_GREEN!✓!C_RESET!"
set "DOT_FAIL=!C_RED!✗!C_RESET!"
set "BAR=!C_DIM!│!C_RESET!"

rem ── Timeline rendering subroutines ─────────────────────────────────────────
rem  Call with: call :step_begin "name"
goto :after_helpers

:step_begin
echo.
echo  %DOT_RUN%  %C_BOLD%%~1%C_RESET%
goto :eof

:step_blank
echo  %BAR%
goto :eof

:step_line
echo  %BAR%  %~1
goto :eof

:step_ok
echo.
echo  %DOT_OK%  %C_BOLD%%~1%C_RESET%
goto :eof

:step_fail
echo  %DOT_FAIL%  %C_BOLD%%~1%C_RESET% 1>&2
goto :eof

:after_helpers

rem ── Main Menu ──────────────────────────────────────────────────────────────
set "INVALID_COUNT=0"

:menu
echo.
echo %C_CYAN%══%C_RESET% %C_BOLD%CIH Setup%C_RESET% %C_CYAN%══%C_RESET%
echo 1) Binary build (cargo build --release)
echo 2) Docker Compose setup
echo q) Quit
echo.

:menu_prompt
set "MENU_CHOICE="
set /p "MENU_CHOICE=Enter choice [1/2/q]: "

if /i "!MENU_CHOICE!"=="1" goto binary_mode
if /i "!MENU_CHOICE!"=="2" goto docker_mode
if /i "!MENU_CHOICE!"=="q" exit /b 0

set /a INVALID_COUNT+=1
if !INVALID_COUNT! geq 3 (
    call :step_fail "ERROR: Too many invalid choices."
    exit /b 1
)
call :step_fail "Invalid choice. Please enter 1, 2, or q."
goto menu_prompt

rem ═════════════════════════════════════════════════════════════════════════════
rem  Binary Build Mode (Option 1)
rem ═════════════════════════════════════════════════════════════════════════════
:binary_mode

echo.
echo  %DOT_RUN%  %C_BOLD%Binary Build Mode%C_RESET%
call :step_blank

rem ── Step 1 — Check cargo prerequisite ───────────────────────────────────────
call :step_begin "Check Rust / Cargo toolchain"
where cargo >nul 2>nul
if %ERRORLEVEL% neq 0 (
    call :step_fail "ERROR: Rust/Cargo not found. Install from https://rustup.rs and try again."
    exit /b 1
)
echo  %BAR%  cargo found
call :step_blank

rem ── Step 2 — Build ──────────────────────────────────────────────────────────
call :step_begin "Build cih-server and cih-engine (release)"
echo  %BAR%  Building...
cargo build --release -p cih-server -p cih-engine
if %ERRORLEVEL% neq 0 (
    call :step_fail "ERROR: cargo build failed. See output above for details."
    exit /b 1
)
call :step_blank

rem ── Step 3 — Verify build artifacts ─────────────────────────────────────────
call :step_begin "Verify build artifacts"
if not exist "%SCRIPT_DIR%target\release\cih-engine.exe" (
    call :step_fail "ERROR: cargo build failed. See output above for details."
    exit /b 1
)
echo  %BAR%  cih-engine : %SCRIPT_DIR%target\release\cih-engine.exe
if not exist "%SCRIPT_DIR%target\release\cih-server.exe" (
    call :step_fail "ERROR: cargo build failed. See output above for details."
    exit /b 1
)
echo  %BAR%  cih-server : %SCRIPT_DIR%target\release\cih-server.exe
call :step_blank

rem ── Step 4 — Offer PATH persistence (default=yes) ──────────────────────────
call :step_begin "Offer PATH persistence"
echo  %BAR%  Binary built at %SCRIPT_DIR%target\release\
echo  %BAR%  Docker deps (FalkorDB, Postgres) are still required at runtime.
echo  %BAR%  Run Docker mode (option 2) or:
echo  %BAR%    docker compose up -d falkordb postgres
call :step_blank

choice /c YN /n /m "Add target/release to your user PATH? [Y/n]: " /t 10 /d Y
if errorlevel 2 goto binary_skip_path
if errorlevel 1 goto binary_add_path

:binary_add_path
echo  %BAR%  Adding to user PATH...

powershell -NoProfile -ExecutionPolicy Bypass -Command ^
  "$releasePath = '%SCRIPT_DIR%target\release'.TrimEnd('\');" ^
  "$current = [Environment]::GetEnvironmentVariable('Path', 'User');" ^
  "if ($current -split ';' ^| Where-Object { $_.TrimEnd('\') -eq $releasePath }) {" ^
  "  Write-Host 'CIH PATH entry already present.';" ^
  "} else {" ^
  "  $new = if ($current) { $current + ';' + $releasePath } else { $releasePath };" ^
  "  [Environment]::SetEnvironmentVariable('Path', $new, 'User');" ^
  "  Write-Host 'CIH PATH entry added: ' + $releasePath;" ^
  "}"
if %ERRORLEVEL% neq 0 (
    echo  %BAR%  %C_YELLOW%WARNING:%C_RESET% Failed to update user PATH. You can add it manually.
)
call :step_ok "Added target/release to your PATH"
echo  %BAR%  Restart your terminal for changes to take effect.
call :step_blank

:binary_skip_path
call :step_ok "Binary Build Mode complete"
goto end

rem ═════════════════════════════════════════════════════════════════════════════
rem  Docker Compose Mode (Option 2)
rem ═════════════════════════════════════════════════════════════════════════════
:docker_mode

echo.
echo  %DOT_RUN%  %C_BOLD%Docker Compose Setup%C_RESET%
call :step_blank

rem ── Step 1 — Check Docker + Docker Compose v2 ──────────────────────────────
call :step_begin "Check Docker and Docker Compose v2"
where docker >nul 2>nul
if %ERRORLEVEL% neq 0 (
    call :step_fail "ERROR: Docker not found. Install Docker Desktop from https://docker.com and try again."
    exit /b 1
)
docker compose version >nul 2>nul
if %ERRORLEVEL% neq 0 (
    call :step_fail "ERROR: Docker Compose v2 not found. Update Docker Desktop and try again."
    exit /b 1
)
echo  %BAR%  docker : OK
echo  %BAR%  compose v2 : OK
call :step_blank

rem ── Step 2 — Prompt for REPO_PATH (max 3 attempts) ─────────────────────────
set "REPO_ATTEMPTS=0"

:prompt_repo_path
set /a REPO_ATTEMPTS+=1
if !REPO_ATTEMPTS! gtr 3 (
    exit /b 1
)

if !REPO_ATTEMPTS! equ 1 call :step_begin "Collect repository path (REPO_PATH)"

set "USER_REPO_PATH="
set /p "USER_REPO_PATH=  %BAR%  REPO_PATH (absolute path to your Java/Spring repo): "

rem Remove surrounding quotes if present
set "USER_REPO_PATH=!USER_REPO_PATH:"=!"

if "!USER_REPO_PATH!"=="" (
    call :step_fail "ERROR: REPO_PATH does not exist: (empty)"
    echo  %BAR%  REPO_PATH must be an absolute path to a Java/Spring repository.
    goto prompt_repo_path
)

if not exist "!USER_REPO_PATH!\" (
    call :step_fail "ERROR: REPO_PATH does not exist: !USER_REPO_PATH!"
    echo  %BAR%  REPO_PATH must be an absolute path to a Java/Spring repository.
    goto prompt_repo_path
)

call :step_ok "REPO_PATH accepted"
echo  %BAR%  REPO_PATH = !USER_REPO_PATH!
call :step_blank

rem ── Step 3 — Prompt for REPO_NAME (optional, default "repo") ───────────────
call :step_begin "Collect repository name (REPO_NAME, optional)"
set "REPO_NAME_INPUT="
set /p "REPO_NAME_INPUT=  %BAR%  REPO_NAME (default: repo): "
if "!REPO_NAME_INPUT!"=="" set "REPO_NAME_INPUT=repo"
echo  %BAR%  REPO_NAME = !REPO_NAME_INPUT!
call :step_blank

rem ── Pass REPO_PATH and REPO_NAME via env vars (avoids shell escaping) ────
set "CIH_SETUP_REPO_PATH=%USER_REPO_PATH%"
set "CIH_SETUP_REPO_NAME=%REPO_NAME_INPUT%"

rem ── Step 4 — Prompt for POSTGRES_PASSWORD (hidden input via PowerShell) ───
set "PG_ATTEMPTS=0"
set "PG_PASSWORD="

call :step_begin "Collect Postgres password (POSTGRES_PASSWORD, hidden)"

:prompt_pg_pass
set /a PG_ATTEMPTS+=1
if !PG_ATTEMPTS! gtr 3 exit /b 1

rem Print the timeline-prefixed prompt without newline, then let PowerShell
rem read the password (no prompt string — Read-Host waits silently).
<nul set /p "=  %BAR%  Postgres password: "
for /f "usebackq delims=" %%p in (`powershell -NoProfile -ExecutionPolicy Bypass -Command "$s=Read-Host -AsSecureString; $b=[Runtime.InteropServices.Marshal]::SecureStringToBSTR($s); $p=[Runtime.InteropServices.Marshal]::PtrToStringAuto($b); [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($b); $p"`) do set "PG_PASSWORD=%%p"

if "!PG_PASSWORD!"=="" (
    call :step_fail "ERROR: POSTGRES_PASSWORD cannot be empty."
    goto :prompt_pg_pass
)

call :step_ok "POSTGRES_PASSWORD accepted"
echo  %BAR%  POSTGRES_PASSWORD = ********
call :step_blank

rem ── Step 5 — Backup existing .env + upsert ────────────────────────────────
call :step_begin "Write .env"

rem Build timestamp (file-safe, from date/time environment vars)
set "TS_DATE=%DATE:/=-%"
set "TS_TIME=%TIME::=-%"
set "TS_TIME=%TS_TIME: =0%"
set "TS_TIME=%TS_TIME:.=%"
set "TS_TIME=%TS_TIME:~0,6%"
set "TIMESTAMP=%TS_DATE%T%TS_TIME%"

if exist "%SCRIPT_DIR%.env" (
    echo  %BAR%  Backing up existing .env to .env.cih-backup-!TIMESTAMP!
    copy /y "%SCRIPT_DIR%.env" "%SCRIPT_DIR%.env.cih-backup-!TIMESTAMP!" >nul
)

echo  %BAR%  Configuring .env...
set "TEMP_ENV=%SCRIPT_DIR%.env.cih-tmp"
del "%TEMP_ENV%" 2>nul

set "SEEN_REPO_PATH=0"
set "SEEN_PG_PASS=0"
set "SEEN_REPO_NAME=0"

if exist "%SCRIPT_DIR%.env" (
    for /f "tokens=1* delims=:" %%i in ('findstr /n "^^" "%SCRIPT_DIR%.env" 2^>nul') do (
        set "CUR_LINE=%%j"
        call :process_env_line
    )
)

rem Append any keys not yet written
if "!SEEN_REPO_PATH!"=="0" >>"%TEMP_ENV%" echo REPO_PATH=!CIH_SETUP_REPO_PATH!
if "!SEEN_PG_PASS!"=="0" >>"%TEMP_ENV%" echo POSTGRES_PASSWORD=!PG_PASSWORD!
if "!SEEN_REPO_NAME!"=="0" >>"%TEMP_ENV%" echo REPO_NAME=!CIH_SETUP_REPO_NAME!

move /y "%TEMP_ENV%" "%SCRIPT_DIR%.env" >nul
if %ERRORLEVEL% neq 0 (
    call :step_fail "ERROR: Failed to write .env. Check permissions and disk space."
    exit /b 1
)
echo  %BAR%  .env written successfully to %SCRIPT_DIR%.env
echo  %BAR%  POSTGRES_PASSWORD=********
call :step_blank
call :step_ok ".env written"
goto :after_env_process

:process_env_line
set "L=!CUR_LINE!"
if "!L!"=="" (
    >>"%TEMP_ENV%" echo.
    goto :eof
)
set "FC=!L:~0,1!"
if "!FC!"=="#" (
    >>"%TEMP_ENV%" echo !L!
    goto :eof
)
for /f "tokens=1,* delims==" %%a in ("!L!") do (
    if "%%a"=="REPO_PATH" (
        if "!SEEN_REPO_PATH!"=="0" (
            >>"%TEMP_ENV%" echo REPO_PATH=!CIH_SETUP_REPO_PATH!
            set "SEEN_REPO_PATH=1"
        )
    ) else if "%%a"=="POSTGRES_PASSWORD" (
        if "!SEEN_PG_PASS!"=="0" (
            >>"%TEMP_ENV%" echo POSTGRES_PASSWORD=!PG_PASSWORD!
            set "SEEN_PG_PASS=1"
        )
    ) else if "%%a"=="REPO_NAME" (
        if "!SEEN_REPO_NAME!"=="0" (
            >>"%TEMP_ENV%" echo REPO_NAME=!CIH_SETUP_REPO_NAME!
            set "SEEN_REPO_NAME=1"
        )
    ) else (
        >>"%TEMP_ENV%" echo !L!
    )
)
goto :eof

:after_env_process

rem ── Step 6 — Validate docker compose config ─────────────────────────────────
call :step_begin "Validate docker compose configuration"
docker compose config >nul 2>nul
if %ERRORLEVEL% neq 0 (
    call :step_fail "ERROR: docker compose configuration is invalid. Check .env and docker-compose.yml."
    exit /b 1
)
echo  %BAR%  Configuration valid.
call :step_blank

rem ── Step 7 — Pull images ────────────────────────────────────────────────────
call :step_begin "Pull Docker images (this can take a while)"
docker compose pull
if %ERRORLEVEL% neq 0 (
    call :step_fail "ERROR: docker compose pull failed. See output above for details."
    exit /b 1
)
call :step_blank

rem ── Step 8 — Start services ─────────────────────────────────────────────────
call :step_begin "Start CIH services"
docker compose up -d
if %ERRORLEVEL% neq 0 (
    call :step_fail "ERROR: docker compose up failed. See output above for details."
    exit /b 1
)
call :step_blank

rem ── Step 9 — Wait for cih-server health ─────────────────────────────────────
call :step_begin "Wait for cih-server to become healthy (timeout: 60s)"

set "HEALTH_COUNT=0"
set "HEALTHY=0"

:health_loop
set /a HEALTH_COUNT+=1
if !HEALTH_COUNT! gtr 12 goto health_timeout

rem Poll every 5 seconds (12 * 5 = 60s max)
timeout /t 5 /nobreak >nul

docker compose ps cih-server 2>nul | findstr /c:"healthy" >nul
if not errorlevel 1 (
    set "HEALTHY=1"
    goto health_ok
)
echo  %BAR%  waiting !HEALTH_COUNT!*5s / 60s ...
goto health_loop

:health_timeout
call :step_blank
echo  %BAR%  %C_YELLOW%WARNING:%C_RESET% cih-server did not become healthy within 60s.
echo  %BAR%           Check: docker compose logs cih-server
goto docker_done

:health_ok
call :step_blank
call :step_ok "Docker Compose Setup complete"

:docker_done
echo  %BAR%  CIH is ready at http://localhost:8080/mcp
echo  %BAR%
echo  %BAR%  Next: see README ^> Quick Start for indexing commands
echo.

goto end

rem ═════════════════════════════════════════════════════════════════════════════
:end
endlocal
exit /b 0