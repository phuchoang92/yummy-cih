#!/usr/bin/env bash
#
# setup.sh — CIH interactive setup wizard (macOS/Linux)
#
# Two modes:
#   1) Binary build  — cargo build --release + optional PATH persistence
#   2) Docker Compose — .env creation + docker compose pull/up + health wait
#
# Output is rendered as a "timeline": each checkpoint is a dot (●) joined by
# connector lines (│). Successful checkpoints show ✓, failed checkpoints ✗.
# Colors are emitted as ANSI escapes when stdout is a TTY and NO_COLOR is unset;
# otherwise everything degrades to plain ASCII glyphs with no escape codes.
#
# Bash 3.2 compatible. No arrays, no [[ ]], no mapfile, no <<<.
#
# Usage:
#   ./setup.sh
#
set -uo pipefail

# ── Resolve repo root ───────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# ── Globals ─────────────────────────────────────────────────────────────────────

SECRET_PASSWORD=""

# ── Color + timeline rendering helpers ──────────────────────────────────────────
# Honor NO_COLOR (https://no-color.org) and non-TTY output.

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    C_RESET=$'\033[0m'
    C_BOLD=$'\033[1m'
    C_DIM=$'\033[2m'
    C_RED=$'\033[31m'
    C_GREEN=$'\033[32m'
    C_YELLOW=$'\033[33m'
    C_CYAN=$'\033[36m'
else
    C_RESET=""
    C_BOLD=""
    C_DIM=""
    C_RED=""
    C_GREEN=""
    C_YELLOW=""
    C_CYAN=""
fi

DOT_RUN="${C_CYAN}●${C_RESET}"
DOT_OK="${C_GREEN}✓${C_RESET}"
DOT_FAIL="${C_RED}✗${C_RESET}"
BAR="${C_DIM}│${C_RESET}"

# Begin a new timeline step: prints ` ●  <name>`
step_begin() {
    printf '\n %s  %s%s%s\n' "$DOT_RUN" "$C_BOLD" "$1" "$C_RESET"
}

# Print a connector row (just ` │`)
step_blank() {
    printf ' %s\n' "$BAR"
}

# Print a sub-line under the current step: ` │  <text>`
step_line() {
    printf ' %s  %s\n' "$BAR" "$1"
}

# Print a yellow-coloured sub-line (for warnings inside a step).
step_warn() {
    printf ' %s  %s%s%s\n' "$BAR" "$C_YELLOW" "$1" "$C_RESET"
}

# Mark the entire phase done with a green check: ` ✓  <name>`
step_ok() {
    printf '\n %s  %s%s%s\n' "$DOT_OK" "$C_BOLD" "$1" "$C_RESET"
}

# Mark the phase failed on stderr with a red cross: ` ✗  <name>`
step_fail() {
    printf '\n %s  %s%s%s\n' "$DOT_FAIL" "$C_BOLD" "$1" "$C_RESET" >&2
}

# Pipe a command's output through this to (a) redact SECRET_PASSWORD and
# (b) prefix every line with ` │  ` so it nests under the active step.
stream_under_step() {
    if [ -n "$SECRET_PASSWORD" ]; then
        awk -v pw="$SECRET_PASSWORD" '{gsub(pw, "********"); print}' \
            | while IFS= read -r line || [ -n "$line" ]; do
                printf ' %s  %s\n' "$BAR" "$line"
            done
    else
        while IFS= read -r line || [ -n "$line" ]; do
            printf ' %s  %s\n' "$BAR" "$line"
        done
    fi
}

# ── Utility functions ───────────────────────────────────────────────────────────

# Read a secret (no echo) from the user.  Sets the global SECRET_PASSWORD.
# The prompt is printed under the active timeline connector (` │  <prompt>`).
read_secret() {
    printf ' %s  %s' "$BAR" "$1"
    trap 'stty echo 2>/dev/null' INT TERM
    stty -echo
    read -r SECRET_PASSWORD
    stty echo
    trap - INT TERM
    printf '\n'
}

# Print an error message to stderr and exit with code 1.
die() {
    step_fail "ERROR: $1"
    exit 1
}

# ── Main flow ───────────────────────────────────────────────────────────────────

check_prereqs() {
    if [ ! -f "$SCRIPT_DIR/docker-compose.yml" ]; then
        die "docker-compose.yml not found. Run this script from the yummy-cih repo root."
    fi
    if [ ! -f "$SCRIPT_DIR/Cargo.toml" ]; then
        die "Cargo.toml not found. Run this script from the yummy-cih repo root."
    fi
}

show_menu() {
    local attempts=0
    local choice=""

    while [ "$attempts" -lt 3 ]; do
        printf '\n'
        printf '%s══%s %sCIH Setup%s %s══%s\n' \
            "$C_CYAN" "$C_RESET" "$C_BOLD" "$C_RESET" "$C_CYAN" "$C_RESET"
        printf '1) Binary build (cargo build --release)\n'
        printf '2) Docker Compose setup\n'
        printf 'q) Quit\n'
        printf '\nChoice: '
        read -r choice

        case "$choice" in
            1)
                binary_mode
                return
                ;;
            2)
                docker_mode
                return
                ;;
            q|Q)
                printf "Goodbye.\n"
                exit 0
                ;;
            *)
                attempts=$((attempts + 1))
                if [ "$attempts" -ge 3 ]; then
                    die "Too many invalid choices."
                fi
                step_fail "Invalid choice. Try again."
                ;;
        esac
    done
}

# ── Binary mode ─────────────────────────────────────────────────────────────────

binary_mode() {
    printf '\n %s  %sBinary Build Mode%s\n' "$DOT_RUN" "$C_BOLD" "$C_RESET"
    step_blank

    # Step 1 — Check cargo
    step_begin "Check Rust / Cargo toolchain"
    if ! command -v cargo >/dev/null 2>&1; then
        die "Rust/Cargo not found. Install from https://rustup.rs and try again."
    fi
    step_line "cargo found: $(command -v cargo)"
    step_blank

    # Step 2 — Build
    step_begin "Build cih-server and cih-engine (release)"
    printf ' %s  Building...\n' "$BAR"
    cargo build --release -p cih-server -p cih-engine 2>&1 | stream_under_step
    if [ "${PIPESTATUS[0]}" -ne 0 ]; then
        die "cargo build failed. See output above for details."
    fi
    step_blank

    # Step 3 — Verify artifacts
    step_begin "Verify build artifacts"
    if [ ! -x "$SCRIPT_DIR/target/release/cih-engine" ]; then
        die "cargo build failed. See output above for details."
    fi
    step_line "cih-engine : $SCRIPT_DIR/target/release/cih-engine"
    if [ ! -x "$SCRIPT_DIR/target/release/cih-server" ]; then
        die "cargo build failed. See output above for details."
    fi
    step_line "cih-server : $SCRIPT_DIR/target/release/cih-server"
    step_blank

    # Step 4 — Offer PATH persistence
    step_begin "Offer PATH persistence"
    step_line "Binary built at $SCRIPT_DIR/target/release/"
    step_line "Docker deps (FalkorDB, Postgres) are still required at runtime."
    step_line "Run Docker mode (option 2) or:"
    step_line "  docker compose up -d falkordb postgres"
    step_blank
    printf ' %s  Add target/release to your shell PATH? [Y/n] ' "$BAR"
    local answer
    read -r answer
    case "$answer" in
        [Nn]|[Nn][Oo])
            step_line "Skipping PATH update."
            step_blank
            ;;
        *)
            update_bash_path
            step_ok "Added target/release to your PATH"
            step_line "Restart your terminal for changes to take effect."
            step_blank
            ;;
    esac

    step_ok "Binary Build Mode complete"
}

# ── Docker mode ─────────────────────────────────────────────────────────────────

docker_mode() {
    printf '\n %s  %sDocker Compose Setup%s\n' "$DOT_RUN" "$C_BOLD" "$C_RESET"
    step_blank

    # Step 1 — Check Docker
    step_begin "Check Docker and Docker Compose v2"
    if ! command -v docker >/dev/null 2>&1; then
        die "Docker not found. Install Docker Desktop from https://docker.com and try again."
    fi
    if ! docker compose version >/dev/null 2>&1; then
        die "Docker Compose v2 not found. Update Docker Desktop and try again."
    fi
    step_line "docker : $(docker --version)"
    step_line "compose: $(docker compose version --short 2>/dev/null || echo 'v2 (short-form unavailable)')"
    step_blank

    # Step 2 — Gather REPO_PATH (up to 3 attempts)
    local repo_path=""
    local attempts=0
    step_begin "Collect repository path (REPO_PATH)"
    while [ "$attempts" -lt 3 ]; do
        printf ' %s  Absolute path to your Java/Spring repository: ' "$BAR"
        read -r repo_path

        if [ -z "$repo_path" ]; then
            step_fail "ERROR: REPO_PATH does not exist: $repo_path"
            step_line "REPO_PATH must be an absolute path to a Java/Spring repository."
            attempts=$((attempts + 1))
            continue
        fi

        # Must be absolute
        case "$repo_path" in
            /*) ;;
            *)
                step_fail "ERROR: REPO_PATH does not exist: $repo_path"
                step_line "REPO_PATH must be an absolute path to a Java/Spring repository."
                attempts=$((attempts + 1))
                continue
                ;;
        esac

        # Must exist
        if [ ! -d "$repo_path" ]; then
            step_fail "ERROR: REPO_PATH does not exist: $repo_path"
            step_line "REPO_PATH must be an absolute path to a Java/Spring repository."
            attempts=$((attempts + 1))
            continue
        fi

        break
    done

    if [ "$attempts" -ge 3 ]; then
        exit 1
    fi
    step_ok "REPO_PATH accepted"
    step_line "REPO_PATH = $repo_path"
    step_blank

    # Step 3 — Gather POSTGRES_PASSWORD (hidden, up to 3 attempts)
    local pg_pass=""
    attempts=0
    step_begin "Collect Postgres password (POSTGRES_PASSWORD, hidden)"
    while [ "$attempts" -lt 3 ]; do
        read_secret "Postgres password: "
        pg_pass="$SECRET_PASSWORD"

        if [ -z "$pg_pass" ]; then
            step_fail "ERROR: POSTGRES_PASSWORD cannot be empty."
            attempts=$((attempts + 1))
            continue
        fi
        break
    done

    if [ "$attempts" -ge 3 ]; then
        exit 1
    fi
    SECRET_PASSWORD="$pg_pass"
    step_ok "POSTGRES_PASSWORD accepted"
    step_line "POSTGRES_PASSWORD = ********"
    step_blank

    # Step 4 — Gather REPO_NAME (optional, default "repo")
    local repo_name=""
    step_begin "Collect repository name (REPO_NAME, optional)"
    printf ' %s  Repository name (slug, default '\''repo'\''): ' "$BAR"
    read -r repo_name
    if [ -z "$repo_name" ]; then
        repo_name="repo"
    fi
    step_line "REPO_NAME = $repo_name"
    step_blank

    # Step 5 — Write/update .env
    step_begin "Write .env"
    if [ -f "$SCRIPT_DIR/.env" ]; then
        local ts
        ts="$(date +%Y-%m-%dT%H%M%S)"
        local backup_path="${SCRIPT_DIR}/.env.cih-backup-${ts}"
        cp "$SCRIPT_DIR/.env" "$backup_path"
        step_line "Backed up existing .env to $backup_path"
    fi
    update_env_file "$SCRIPT_DIR/.env" "$repo_path" "$pg_pass" "$repo_name" 2>&1 | stream_under_step
    step_line ".env written to $SCRIPT_DIR/.env"
    step_line "POSTGRES_PASSWORD=********"
    step_blank

    # Step 6 — Validate
    step_begin "Validate docker compose configuration"
    if ! docker compose config >/dev/null 2>&1; then
        die "docker compose configuration is invalid. Check .env and docker-compose.yml."
    fi
    step_line "Configuration valid."
    step_blank

    # Step 7 — Pull images
    step_begin "Pull Docker images (this can take a while)"
    docker compose pull 2>&1 | stream_under_step
    if [ "${PIPESTATUS[0]}" -ne 0 ]; then
        die "docker compose pull failed. See output above for details."
    fi
    step_blank

    # Step 8 — Start services
    step_begin "Start CIH services"
    docker compose up -d 2>&1 | stream_under_step
    if [ "${PIPESTATUS[0]}" -ne 0 ]; then
        die "docker compose up failed. See output above for details."
    fi
    step_blank

    # Step 9 — Wait for healthy
    step_begin "Wait for cih-server to become healthy (timeout: 60s)"
    local timeout=60
    local elapsed=0
    local interval=2

    while [ "$elapsed" -lt "$timeout" ]; do
        if docker compose ps 2>/dev/null | grep -q "cih-server.*healthy"; then
            step_ok "Docker Compose Setup complete"
            printf ' %s  %sCIH is ready at http://localhost:8080/mcp%s\n' \
                "$BAR" "$C_BOLD" "$C_RESET"
            printf ' %s\n' "$BAR"
            printf ' %s  Next: see README → Quick Start for indexing commands\n' "$BAR"
            return 0
        fi
        step_line "waiting ${elapsed}s / ${timeout}s ..."
        sleep "$interval"
        elapsed=$((elapsed + interval))
    done

    step_blank
    step_warn "WARNING: cih-server did not become healthy within 60s."
    step_line "Check: docker compose logs cih-server"
}

# ── PATH update (idempotent, CIH-marked block) ─────────────────────────────────

update_bash_path() {
    # Step 1 — find or create shell profile
    local profile=""
    local found=0

    for candidate in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.bash_profile" "$HOME/.profile"; do
        if [ -f "$candidate" ] && [ -w "$candidate" ]; then
            profile="$candidate"
            found=1
            break
        fi
    done

    if [ "$found" -eq 0 ]; then
        profile="$HOME/.bashrc"
        touch "$profile"
    fi

    local release_dir="$SCRIPT_DIR/target/release"
    local open_marker="# >>> CIH begin >>>"
    local close_marker="# <<< CIH end <<<"

    # Step 2 — read existing content
    local temp_file="${profile}.cih-tmp-$$"
    local has_open=0
    local has_close=0
    local open_line=0
    local close_line=0
    local line_num=0

    rm -f "$temp_file"

    # First pass: find marker positions
    if [ -f "$profile" ]; then
        while IFS= read -r line || [ -n "$line" ]; do
            line_num=$((line_num + 1))
            if [ "$line" = "$open_marker" ] && [ "$has_open" -eq 0 ]; then
                has_open=1
                open_line="$line_num"
            fi
            if [ "$has_open" -eq 1 ] && [ "$line" = "$close_marker" ] && [ "$has_close" -eq 0 ]; then
                has_close=1
                close_line="$line_num"
            fi
        done < "$profile"
    fi

    # Step 3 — build output
    if [ "$has_open" -eq 1 ] && [ "$has_close" -eq 1 ] && [ "$close_line" -gt "$open_line" ]; then
        # REPLACE MODE: remove old CIH block, strip trailing blanks, append new block
        line_num=0
        {
            while IFS= read -r line || [ -n "$line" ]; do
                line_num=$((line_num + 1))
                if [ "$line_num" -lt "$open_line" ] || [ "$line_num" -gt "$close_line" ]; then
                    printf '%s\n' "$line"
                fi
            done < "$profile"
        } > "$temp_file"

        # Strip trailing blank lines (portable awk, no sed -i needed)
        awk 'NF {last=NR} {lines[NR]=$0} END {for(i=1;i<=last;i++) print lines[i]}' \
            "$temp_file" > "${temp_file}.awk" && mv "${temp_file}.awk" "$temp_file"

        # Append blank line separator + new block
        printf '\n' >> "$temp_file"
        printf '%s\n' "$open_marker" >> "$temp_file"
        printf 'export PATH="%s:$PATH"\n' "$release_dir" >> "$temp_file"
        printf '%s\n' "$close_marker" >> "$temp_file"
    else
        # APPEND MODE: copy all content, strip trailing blanks, append new block
        if [ -f "$profile" ]; then
            cp "$profile" "$temp_file"

            # Strip trailing blank lines (portable awk)
            awk 'NF {last=NR} {lines[NR]=$0} END {for(i=1;i<=last;i++) print lines[i]}' \
                "$temp_file" > "${temp_file}.awk" && mv "${temp_file}.awk" "$temp_file"
        else
            rm -f "$temp_file"
            touch "$temp_file"
        fi

        # Append blank line + new block
        printf '\n' >> "$temp_file"
        printf '%s\n' "$open_marker" >> "$temp_file"
        printf 'export PATH="%s:$PATH"\n' "$release_dir" >> "$temp_file"
        printf '%s\n' "$close_marker" >> "$temp_file"
    fi

    # Step 4 — write back
    mv "$temp_file" "$profile"
}

# ── .env backup & upsert (idempotent) ──────────────────────────────────────────

update_env_file() {
    local env_path="$1"
    local repo_path="$2"
    local pg_pass="$3"
    local repo_name="$4"

    # Step 2 & 3 — process line by line (backup handled by caller in timeline)
    local temp_env="${env_path}.cih-tmp-$$"
    rm -f "$temp_env"

    local seen_repo_path=0
    local seen_pg_pass=0
    local seen_repo_name=0

    if [ -f "$env_path" ]; then
        while IFS= read -r line || [ -n "$line" ]; do
            case "$line" in
                "#"*|"")
                    # Preserve comments and blank lines verbatim
                    printf '%s\n' "$line" >> "$temp_env"
                    ;;
                REPO_PATH=*)
                    if [ "$seen_repo_path" -eq 0 ]; then
                        printf 'REPO_PATH=%s\n' "$repo_path" >> "$temp_env"
                        seen_repo_path=1
                    fi
                    # Skip duplicates
                    ;;
                POSTGRES_PASSWORD=*)
                    if [ "$seen_pg_pass" -eq 0 ]; then
                        printf 'POSTGRES_PASSWORD=%s\n' "$pg_pass" >> "$temp_env"
                        seen_pg_pass=1
                    fi
                    # Skip duplicates
                    ;;
                REPO_NAME=*)
                    if [ "$seen_repo_name" -eq 0 ]; then
                        printf 'REPO_NAME=%s\n' "$repo_name" >> "$temp_env"
                        seen_repo_name=1
                    fi
                    # Skip duplicates
                    ;;
                *)
                    # Preserve unknown keys and malformed lines verbatim
                    printf '%s\n' "$line" >> "$temp_env"
                    ;;
            esac
        done < "$env_path"
    fi

    # Step 4 — append any required keys not yet written
    if [ "$seen_repo_path" -eq 0 ]; then
        printf 'REPO_PATH=%s\n' "$repo_path" >> "$temp_env"
    fi
    if [ "$seen_pg_pass" -eq 0 ]; then
        printf 'POSTGRES_PASSWORD=%s\n' "$pg_pass" >> "$temp_env"
    fi
    if [ "$seen_repo_name" -eq 0 ]; then
        printf 'REPO_NAME=%s\n' "$repo_name" >> "$temp_env"
    fi

    # Step 5 — write back
    mv "$temp_env" "$env_path"
}

# ── Entry point ─────────────────────────────────────────────────────────────────

main() {
    check_prereqs
    show_menu
}

main