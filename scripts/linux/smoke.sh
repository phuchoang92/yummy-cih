#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "Usage: smoke.sh --cih FILE --fixture DIR --work-dir DIR [--port PORT]"
}

cih=
fixture=
work_dir=
port=18080
while (($#)); do
  case "$1" in
    --cih) cih=${2:?}; shift 2 ;;
    --fixture) fixture=${2:?}; shift 2 ;;
    --work-dir) work_dir=${2:?}; shift 2 ;;
    --port) port=${2:?}; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

for command in awk curl jq; do
  command -v "$command" >/dev/null || { echo "$command is required" >&2; exit 1; }
done

mcp_session_id() {
  awk 'tolower($1) == "mcp-session-id:" { sub(/\r$/, "", $2); print $2; exit }'
}

case_probe=$(printf 'mCp-SeSsIoN-Id: portable-session\r\n' | mcp_session_id)
[[ $case_probe == portable-session ]] || {
  echo "awk cannot parse case-insensitive MCP session headers" >&2
  exit 1
}
cih=$(realpath "$cih")
fixture=$(realpath "$fixture")
mkdir -p "$work_dir"
work_dir=$(realpath "$work_dir")
export CIH_HOME="$work_dir/CIH Home 数据"
repo="$work_dir/fixture repo 日本語 $(printf 'x%.0s' {1..72})"
second_repo="$work_dir/second fixture"
mkdir -p "$repo" "$second_repo" "$CIH_HOME"
cp -a "$fixture/." "$repo/"
cp -a "$fixture/." "$second_repo/"

server_pid=
ambiguous_pid=
conflict_pid=
cleanup() {
  for pid in "$conflict_pid" "$server_pid" "$ambiguous_pid"; do
    [[ -n $pid ]] && kill "$pid" 2>/dev/null || true
  done
}
trap cleanup EXIT

"$cih" doctor >/dev/null

if [[ $(id -u) -ne 0 ]]; then
  readonly_home="$work_dir/read only home"
  mkdir -p "$readonly_home"
  chmod 500 "$readonly_home"
  if CIH_HOME="$readonly_home" "$cih" doctor --json >"$work_dir/read-only-doctor.json" 2>/dev/null; then
    echo "doctor accepted a read-only CIH_HOME" >&2
    exit 1
  fi
  jq -e '.home.ok == false' "$work_dir/read-only-doctor.json" >/dev/null
  chmod 700 "$readonly_home"
fi

"$cih" index "$repo" --force
for path in "$CIH_HOME/registry.json" "$repo/.cih/repository-identity.json" "$repo/.cih/wiki"; do
  [[ -e $path ]] || { echo "missing index output: $path" >&2; exit 1; }
done

"$cih" index "$second_repo" --force --no-wiki
(cd "$work_dir" && "$cih" serve >"$work_dir/ambiguous.log" 2>&1) &
ambiguous_pid=$!
for _ in {1..20}; do
  kill -0 "$ambiguous_pid" 2>/dev/null || break
  sleep 0.1
done
if kill -0 "$ambiguous_pid" 2>/dev/null; then
  echo "cih serve without a repo did not reject an ambiguous registry" >&2
  exit 1
fi
if wait "$ambiguous_pid"; then
  echo "cih serve accepted an ambiguous registry" >&2
  exit 1
fi
ambiguous_pid=

"$cih" serve "$repo" --bind "127.0.0.1:$port" >"$work_dir/server.log" 2>&1 &
server_pid=$!
ready=0
for _ in {1..120}; do
  kill -0 "$server_pid" 2>/dev/null || {
    cat "$work_dir/server.log" >&2
    echo "cih serve exited before readiness" >&2
    exit 1
  }
  if curl -fsS "http://127.0.0.1:$port/ready" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.25
done
((ready)) || { echo "cih serve did not become ready" >&2; exit 1; }

"$cih" serve "$repo" --bind "127.0.0.1:$port" >"$work_dir/conflict.log" 2>&1 &
conflict_pid=$!
for _ in {1..50}; do
  kill -0 "$conflict_pid" 2>/dev/null || break
  sleep 0.1
done
if kill -0 "$conflict_pid" 2>/dev/null; then
  echo "second server did not reject the occupied port" >&2
  exit 1
fi
if wait "$conflict_pid"; then
  echo "second server accepted the occupied port" >&2
  exit 1
fi
conflict_pid=

for route in health ready graph; do
  curl -fsS "http://127.0.0.1:$port/$route" >/dev/null
done

headers=$(mktemp "$work_dir/headers.XXXXXXXX")
initialize='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"linux-smoke","version":"1"}}}'
curl -fsS -D "$headers" -o "$work_dir/initialize.json" \
  -H 'Accept: application/json, text/event-stream' -H 'Content-Type: application/json' \
  -d "$initialize" "http://127.0.0.1:$port/mcp"
session=$(mcp_session_id <"$headers")
[[ -n $session ]] || { echo "MCP initialize returned no session id" >&2; exit 1; }

mcp_post() {
  local body=$1 output=$2
  curl -fsS -o "$output" \
    -H 'Accept: application/json, text/event-stream' -H 'Content-Type: application/json' \
    -H "Mcp-Session-Id: $session" -d "$body" "http://127.0.0.1:$port/mcp"
}
mcp_post '{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}' "$work_dir/initialized.json"
mcp_post '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' "$work_dir/tools.json"
grep -q 'search_code' "$work_dir/tools.json"
grep -q 'query' "$work_dir/tools.json"

for call in \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_code","arguments":{"query":"order service","limit":5}}}' \
  '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"query","arguments":{"q":"order service","limit":5}}}' \
  '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"communities","arguments":{"limit":10}}}'; do
  id=$(jq -r '.id' <<<"$call")
  mcp_post "$call" "$work_dir/call-$id.json"
  if grep -Eq '"isError"[[:space:]]*:[[:space:]]*true' "$work_dir/call-$id.json"; then
    echo "MCP tool call $id returned an error" >&2
    exit 1
  fi
done
grep -q 'node_id' "$work_dir/call-3.json"
grep -q 'bm25' "$work_dir/call-4.json"

"$cih" index "$repo" --force --no-wiki
curl -fsS "http://127.0.0.1:$port/ready" >/dev/null

kill "$server_pid"
wait "$server_pid" || true
server_pid=
echo "${CIH_SMOKE_PLATFORM:-Linux} portable smoke test passed"
