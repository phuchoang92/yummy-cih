#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
bench_root="${CIH_PROJECTION_BENCH_DIR:-$repo_root/target/cih-projection-bench}"
results_dir="$bench_root/results"
mkdir -p "$results_dir"

run_scale() {
  local nodes="$1"
  cargo run --release -p cih-server --example scale_bench -- \
    --fixture-dir "$bench_root/synthetic-${nodes}" \
    --nodes "$nodes" --edges-per-node 2 --iterations 20 \
    --burst-callers 16 --search-cache-bytes 1 \
    --output "$results_dir/synthetic-${nodes}.json" --enforce
}

run_linux() {
  local linux_dir="$bench_root/linux-v7.2"
  local linux_commit="237a1c39e8dfd3e1c6f1f023eea37a48ec04cc63"
  if [[ ! -d "$linux_dir/.git" ]]; then
    git clone --filter=blob:none --no-checkout https://github.com/torvalds/linux.git "$linux_dir"
  fi
  git -C "$linux_dir" fetch --depth 1 origin "$linux_commit"
  git -C "$linux_dir" checkout --detach "$linux_commit"

  cargo build --release -p cih-cli
  local cih_bin="$repo_root/target/release/cih"
  local cih_data="$bench_root/cih-home"
  CIH_HOME="$cih_data" "$cih_bin" index "$linux_dir" \
    --grouping package --no-wiki --no-agent-context --json \
    > "$results_dir/linux-index.json"

  CIH_HOME="$cih_data" "$cih_bin" serve "$linux_dir" --bind 127.0.0.1:18080 \
    > "$results_dir/linux-server.log" 2>&1 &
  local server_pid=$!
  trap 'kill "$server_pid" 2>/dev/null || true' RETURN
  for _ in {1..120}; do
    curl --fail --silent http://127.0.0.1:18080/readyz >/dev/null && break
    sleep 0.25
  done
  curl --fail --silent --show-error \
    --write-out '%{time_starttransfer} %{time_total} %{size_download}\n' \
    --output "$results_dir/linux-repository-projection.json" \
    'http://127.0.0.1:18080/api/graph/projection?scope=repository&max_response_bytes=1048576' \
    > "$results_dir/linux-repository-projection-timing.txt"
  jq -e '.nodes | length <= 10000' "$results_dir/linux-repository-projection.json" >/dev/null
  jq -e '.edges | length <= 50000' "$results_dir/linux-repository-projection.json" >/dev/null
  test "$(wc -c < "$results_dir/linux-repository-projection.json")" -le 1048576
  kill "$server_pid"
  wait "$server_pid" 2>/dev/null || true
  trap - RETURN
}

case "${1:-all}" in
  synthetic-500k) run_scale 500000 ;;
  synthetic-1m) run_scale 1000000 ;;
  linux) run_linux ;;
  all) run_scale 500000; run_scale 1000000; run_linux ;;
  *) echo "usage: $0 [synthetic-500k|synthetic-1m|linux|all]" >&2; exit 2 ;;
esac
