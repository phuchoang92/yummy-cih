#!/usr/bin/env bash
#
# Serve the whole 212ecom (dienmaychiben) microservice from ONE CIH MCP server.
#
# cih-server is multi-repo: it fronts the `dienmaychiben` group (CIH_GROUP), so
# a single endpoint answers every service. The primary graph (be) backs deep
# tools when no `repo` arg is given; pass `repo=212ecom-fe` / `repo=212ecom-ai`
# to target the TypeScript / Python services, and the cross-repo tools
# (list_repos, trace_flow_x, api_impact, shape_check) span all three. Member
# graphs all live in the FalkorDB the compose stack exposes on host :6380, so
# one server reaches them by connecting per graph key on demand.
# See docs/runbooks/multi-repo-host-serving.md.
#
# Usage:
#   scripts/serve-212ecom.sh [start]   # (re)launch — idempotent
#   scripts/serve-212ecom.sh stop
#   scripts/serve-212ecom.sh status
#
# Prereqs: FalkorDB on :6380 and (optional) pgvector on :5433 — the compose
# `falkordb`/`postgres` services. Members analyzed+discovered on the host (valid
# registry artifacts_dir) and grouped: `cih-engine group sync dienmaychiben`.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO_ROOT/target/release/cih-server"
LOG_DIR="${CIH_SERVE_LOG_DIR:-/tmp/cih-212ecom}"
STACK_ROOT="${CIH_212ECOM_ROOT:-/Users/phuc/BigMoves/dienmaychiben}"
FALKOR_URL="${FALKOR_URL:-redis://127.0.0.1:6380}"

# One server fronting the group. Primary = be (Java); pass repo= for fe/ai.
PORT="${CIH_212ECOM_PORT:-8080}"
GROUP="dienmaychiben"
PRIMARY_KEY="212ecom_be"
PRIMARY_ARTIFACTS="$STACK_ROOT/212ecom-be/.cih/artifacts"
PIDFILE="$LOG_DIR/cih-212ecom.pid"
LOGFILE="$LOG_DIR/cih-212ecom.log"

# CIH_PG_URL from the compose .env (optional — enables semantic query /
# ask_codebase; BM25 search_code works without it).
pg_url() {
  local env_file="$REPO_ROOT/.env"
  [ -f "$env_file" ] || return 0
  local user pass db
  user="$(grep -E '^POSTGRES_USER=' "$env_file" | cut -d= -f2-)"; user="${user:-cih}"
  pass="$(grep -E '^POSTGRES_PASSWORD=' "$env_file" | cut -d= -f2-)"
  db="$(grep -E '^POSTGRES_DB=' "$env_file" | cut -d= -f2-)"; db="${db:-cih}"
  [ -n "$pass" ] || return 0
  printf 'postgres://%s:%s@127.0.0.1:5433/%s' "$user" "$pass" "$db"
}

free_port() {
  local pids
  pids="$(lsof -ti "tcp:$1" -sTCP:LISTEN 2>/dev/null || true)"
  [ -z "$pids" ] || { echo "  freeing :$1 (kill $pids)"; kill $pids 2>/dev/null || true; sleep 1; }
}

start() {
  [ -x "$BIN" ] || { echo "error: $BIN not found — run: cargo build --release -p cih-server" >&2; exit 1; }
  [ -d "$PRIMARY_ARTIFACTS" ] || { echo "error: no artifacts at $PRIMARY_ARTIFACTS (analyze 212ecom-be first)" >&2; exit 1; }
  mkdir -p "$LOG_DIR"
  local pg; pg="$(pg_url || true)"
  export FALKOR_URL
  if [ -n "$pg" ]; then export CIH_PG_URL="$pg"; echo "pgvector: enabled (:5433)"; else unset CIH_PG_URL || true; echo "pgvector: disabled (no POSTGRES_PASSWORD in .env) — semantic query off"; fi
  echo "FalkorDB: $FALKOR_URL"
  free_port "$PORT"

  CIH_BIND="127.0.0.1:$PORT" \
  CIH_GRAPH_KEY="$PRIMARY_KEY" \
  CIH_GROUP="$GROUP" \
  CIH_ARTIFACTS_DIR="$PRIMARY_ARTIFACTS" \
  RUST_LOG="${RUST_LOG:-info,cih_server=info}" \
    nohup "$BIN" >"$LOGFILE" 2>&1 &
  echo "$!" >"$PIDFILE"
  echo "  started cih-212ecom → http://127.0.0.1:$PORT/mcp  (group=$GROUP, primary=$PRIMARY_KEY, pid=$!)"
  echo
  echo "waiting for health..."
  sleep 2
  status
  echo
  echo "one endpoint serves the whole stack:"
  echo "  route_map {}                       → be (Java, primary)"
  echo "  route_map {\"repo\":\"212ecom-ai\"}    → Python FastAPI routes"
  echo "  context  {\"name\":..,\"repo\":\"212ecom-fe\"} → TypeScript"
  echo "  trace_flow_x / api_impact          → span all three"
}

stop() {
  if [ -f "$PIDFILE" ] && kill "$(cat "$PIDFILE")" 2>/dev/null; then
    echo "stopped cih-212ecom (pid $(cat "$PIDFILE"))"
  else
    free_port "$PORT"
  fi
  rm -f "$PIDFILE"
}

status() {
  local code
  code="$(curl -s -o /dev/null -w '%{http_code}' -m 2 "http://127.0.0.1:$PORT/health" 2>/dev/null || echo 000)"
  printf '  cih-212ecom :%s  group=%s  primary=%s  health=%s\n' "$PORT" "$GROUP" "$PRIMARY_KEY" "$code"
}

case "${1:-start}" in
  start) start ;;
  stop)  stop ;;
  status) status ;;
  *) echo "usage: $0 [start|stop|status]" >&2; exit 2 ;;
esac
