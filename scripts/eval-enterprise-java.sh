#!/usr/bin/env bash
#
# Phase 1 enterprise-Java eval harness.
#
# Runs `cih-engine analyze --all --no-load` over a set of well-known enterprise
# Java repos and asserts that route / integration extraction meets baselines:
#   - spring-petclinic : Spring MVC route count   >= 20
#   - apache/fineract  : JAX-RS route count       >= 100
#   - apache/servicemix: IntegrationRoute nodes    > 0  (Camel/Blueprint/Spring XML)
#
# Repos are expected under $EVAL_REPOS_DIR (default ~/eval-repos):
#   spring-petclinic/  fineract/  servicemix/
#
# Usage:
#   scripts/eval-enterprise-java.sh
#   EVAL_REPOS_DIR=/path/to/repos scripts/eval-enterprise-java.sh
#
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EVAL_REPOS_DIR="${EVAL_REPOS_DIR:-$HOME/eval-repos}"

PETCLINIC_MIN="${PETCLINIC_MIN:-20}"
FINERACT_MIN="${FINERACT_MIN:-100}"
SERVICEMIX_MIN="${SERVICEMIX_MIN:-1}"

# repo dir name -> expected location under EVAL_REPOS_DIR
PETCLINIC_DIR="$EVAL_REPOS_DIR/spring-petclinic"
FINERACT_DIR="$EVAL_REPOS_DIR/fineract"
SERVICEMIX_DIR="$EVAL_REPOS_DIR/servicemix"

FAILURES=0
declare -a SUMMARY

color() { # $1=code $2=text
    if [ -t 1 ]; then printf '\033[%sm%s\033[0m' "$1" "$2"; else printf '%s' "$2"; fi
}
pass() { color "32" "PASS"; }
fail() { color "31" "FAIL"; }
warn() { color "33" "WARN"; }

build_cli() {
    echo "Building cih-engine (release)..."
    if ! cargo build --release -p cih-engine --bin cih-engine >/dev/null 2>&1; then
        echo "ERROR: failed to build cih-engine" >&2
        exit 1
    fi
    CIH_BIN="$REPO_ROOT/target/release/cih-engine"
}

# Find the newest nodes.jsonl under <repo>/.cih/artifacts/*/
latest_nodes_jsonl() {
    local repo="$1"
    local dir
    dir="$(ls -1dt "$repo"/.cih/artifacts/*/ 2>/dev/null | head -n1)"
    [ -n "$dir" ] && [ -f "${dir}nodes.jsonl" ] && printf '%s' "${dir}nodes.jsonl"
}

# Count nodes whose kind matches $2; optionally only those whose props.source
# JSON value matches $3 (a route "source" like spring_mvc / jax_rs).
count_kind() {
    local nodes_file="$1" kind="$2" source="${3:-}"
    if [ -z "$source" ]; then
        grep -c "\"kind\":\"$kind\"" "$nodes_file" 2>/dev/null || echo 0
    else
        # Lines that are Route nodes with the requested source.
        grep "\"kind\":\"$kind\"" "$nodes_file" 2>/dev/null \
            | grep -c "\"source\":\"$source\"" || echo 0
    fi
}

run_repo() { # $1=label $2=dir
    local label="$1" dir="$2"
    if [ ! -d "$dir" ]; then
        echo "[$(warn)] $label: repo not found at $dir (skipping)"
        SUMMARY+=("$(warn) $label: missing repo ($dir)")
        return 2
    fi
    echo "Analyzing $label ($dir)..."
    if ! "$CIH_BIN" analyze "$dir" --all --no-load --no-cache >/dev/null 2>&1; then
        echo "[$(fail)] $label: analyze command failed"
        SUMMARY+=("$(fail) $label: analyze failed")
        FAILURES=$((FAILURES + 1))
        return 1
    fi
    return 0
}

assert_ge() { # $1=label $2=actual $3=min $4=metric
    local label="$1" actual="$2" min="$3" metric="$4"
    if [ "$actual" -ge "$min" ]; then
        echo "[$(pass)] $label: $metric = $actual (>= $min)"
        SUMMARY+=("$(pass) $label: $metric=$actual (min $min)")
    else
        echo "[$(fail)] $label: $metric = $actual (< $min)"
        SUMMARY+=("$(fail) $label: $metric=$actual (min $min)")
        FAILURES=$((FAILURES + 1))
    fi
}

main() {
    build_cli

    # --- spring-petclinic: Spring MVC routes ------------------------------
    if run_repo "spring-petclinic" "$PETCLINIC_DIR"; then
        nodes="$(latest_nodes_jsonl "$PETCLINIC_DIR")"
        if [ -n "$nodes" ]; then
            routes="$(count_kind "$nodes" Route)"
            assert_ge "spring-petclinic" "$routes" "$PETCLINIC_MIN" "routes"
        else
            echo "[$(fail)] spring-petclinic: no nodes.jsonl artifact found"
            SUMMARY+=("$(fail) spring-petclinic: no artifact")
            FAILURES=$((FAILURES + 1))
        fi
    fi

    # --- fineract: JAX-RS routes ------------------------------------------
    if run_repo "fineract" "$FINERACT_DIR"; then
        nodes="$(latest_nodes_jsonl "$FINERACT_DIR")"
        if [ -n "$nodes" ]; then
            jaxrs="$(count_kind "$nodes" Route jax_rs)"
            assert_ge "fineract" "$jaxrs" "$FINERACT_MIN" "jax_rs routes"
        else
            echo "[$(fail)] fineract: no nodes.jsonl artifact found"
            SUMMARY+=("$(fail) fineract: no artifact")
            FAILURES=$((FAILURES + 1))
        fi
    fi

    # --- servicemix: IntegrationRoute nodes -------------------------------
    if run_repo "servicemix" "$SERVICEMIX_DIR"; then
        nodes="$(latest_nodes_jsonl "$SERVICEMIX_DIR")"
        if [ -n "$nodes" ]; then
            integ="$(count_kind "$nodes" IntegrationRoute)"
            assert_ge "servicemix" "$integ" "$SERVICEMIX_MIN" "integration routes"
        else
            echo "[$(fail)] servicemix: no nodes.jsonl artifact found"
            SUMMARY+=("$(fail) servicemix: no artifact")
            FAILURES=$((FAILURES + 1))
        fi
    fi

    echo
    echo "==================== EVAL SUMMARY ===================="
    for line in "${SUMMARY[@]}"; do
        echo "  $line"
    done
    echo "======================================================"

    if [ "$FAILURES" -gt 0 ]; then
        echo "Result: $(fail) ($FAILURES failing assertion(s))"
        exit 1
    fi
    echo "Result: $(pass)"
}

main "$@"
