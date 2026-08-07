# Platform retrieval production acceptance

Status: **runner ready; target-host evidence pending**.

This record closes the production-only gates in
`docs/plans/search-index-scale-performance.md`. Synthetic 500k acceptance is
already recorded in `docs/perf/scale-500k.md`; it does not replace this run.

## Preconditions

1. Deploy revision `6921777` or a descendant containing the retrieval changes.
2. Run analyze for `platform` and verify the newest complete artifact directory
   contains a non-empty `search-index.bin`.
3. Configure cache budgets for the measured hot set and keep
   `CIH_SEARCH_SIDECAR_ENABLED=1`.
4. Restart `cih-server`, then run the acceptance command before any client sends
   a search request. The cold gate expects exactly one sidecar load and zero
   fallback builds.

## Command

Run from a checkout that can reach the server and read the artifact mount:

```bash
CIH_API_TOKEN='<token-if-configured>' \
python3 scripts/validate-retrieval-production.py \
  --server-url http://127.0.0.1:8080 \
  --artifacts-dir /workspace/platform/.cih/artifacts \
  --repository-label platform \
  --output docs/perf/search-platform-474k.json
```

Omit `CIH_API_TOKEN` when the target is intentionally unauthenticated. If the
script runs on the Docker host rather than inside the server container, replace
`--artifacts-dir` with the host path mounted at `/workspace/platform`.
The runner records the checkout's current Git revision automatically; use
`CIH_ACCEPT_REVISION` only when the deployed server came from a different
checkout.

The runner performs and records only bounded metadata for:

- MCP initialization and non-empty tool discovery;
- 16 simultaneous cold searches with sidecar/load/build counter deltas;
- retained index size and document count from `/operations/metrics`;
- 16 simultaneous warm searches, scorer scratch, and health latency;
- overview with and without the optional wiki section;
- scoped Java grep for `CustomRecTransfers`;
- a deliberate no-match Java grep to exercise the worst-case deadline;
- final search, wiki, and grep operational counters.

Tool result bodies and source text are not written to the report.

## Local MCP Preflight

The runner was exercised end to end on 2026-07-22 against an isolated Ladybug
copy of the Fineract snapshot (87,280 nodes, 253,144 edges). This validates the
MCP protocol, metrics, cold/warm search, overview, and grep paths, but it does
not replace the target-host `platform` run.

| Measurement | Observed |
|---|---:|
| Persisted sidecar | 32,284,898 bytes |
| Retained search index | 37,515,232 bytes |
| Cold 16-caller burst | 77.433 ms, one sidecar load |
| Warm 16-caller p95 | 3.740 ms, 16 cache hits |
| Scorer scratch high-water | 1,251,056 bytes aggregate; 312,764 bytes/query |
| Event-loop health p99 | 0.375 ms cold; 0.276 ms warm |
| Overview | 269.829 ms without wiki; 250.244 ms default |
| Grep | 210.565 ms scoped; 176.862 ms full no-match |

The sanitized machine-readable record is
`docs/perf/search-fineract-local.json`.

## Required Result

The command must exit zero and every JSON gate must pass. In particular:

| Gate | Target |
|---|---|
| Compact retained BM25 | `<= 230 MiB`, retained |
| Valid cold sidecar | exactly one load, zero fallback builds |
| Cold 16-caller burst | `<= 10 s`, identical results |
| Warm 16-caller burst | p95 `<= 500 ms`, no reload/build |
| Event-loop health | p99 `< 50 ms` during cold and warm bursts |
| Scorer scratch | `<= 6 MiB` per active scorer, `<= 32 MiB` aggregate |
| Overview | `<= 2 s` with and without optional wiki handling |
| Scoped Java grep | `<= 10 s` |
| Worst-case grep | complete or explicit partial response `<= 85 s` |

After this passes, run the production 30-minute mixed soak and alternating test
against eight distinct repositories. Those are the final rollout gates before
removing the sidecar rollback switch.

## Eight-repository production soak

The scheduled `cih-server-soak.yml` workflow is a synthetic regression test. It
does not satisfy the production gate because it repeatedly drives generated
artifacts rather than one long-lived MCP server with eight real repositories.
Use `scripts/validate-retrieval-production-soak.py` for the rollout decision.

Copy
`docs/perf/retrieval-production-soak-manifest.example.json` outside the checkout,
then replace every placeholder with one registered production repository. Each
entry needs a query and grep pattern known to exist in that repository. The
runner verifies that the eight selectors, canonical artifact roots, and full
`nodes.jsonl` hashes are distinct; hard-linked or copied synthetic fixtures do
not pass. Keep the completed manifest outside version control because it
contains production selectors, paths, queries, and grep patterns.

Size the final server from decoded `search_indexes[*].index_bytes`, not
`search-index.bin` file sizes. On a temporary lexical-only server with a safe
high cache budget, search each repository once, sum its eight observed index
sizes, and configure at least 10% headroom. The total cache-family budget must
also include the artifact, wiki, and resource-index cache ceilings. Stop that
measurement server before acceptance.

```text
CIH_SEARCH_CACHE_MAX_BYTES >= ceil(sum(index_bytes) * 1.10)
CIH_CACHE_MAX_BYTES >= CIH_ARTIFACT_CACHE_MAX_BYTES
                     + CIH_WIKI_CACHE_MAX_BYTES
                     + CIH_SEARCH_CACHE_MAX_BYTES
                     + CIH_RESOURCE_INDEX_CACHE_MAX_BYTES
```

Start a fresh, otherwise idle Linux server with the final decimal-byte budgets.
The runner must be the same user and share the PID, network, and mount namespaces
with `cih-server` so it can read `/proc/<pid>/{cmdline,environ,status}` and the
same artifacts. It accepts only a direct `http://127.0.0.1` URL and verifies
that the server process owns that IPv4 loopback (or wildcard) listening port:

```bash
: "${CIH_SEARCH_CACHE_MAX_BYTES:?set sum(index_bytes) plus 10 percent first}"
: "${CIH_CACHE_MAX_BYTES:?set the total cache-family budget first}"

env -u CIH_PG_URL \
  CIH_SEARCH_SIDECAR_ENABLED=1 \
  CIH_SEARCH_CACHE_MAX_ENTRIES=8 \
  CIH_SEARCH_CACHE_MAX_BYTES="$CIH_SEARCH_CACHE_MAX_BYTES" \
  CIH_CACHE_MAX_BYTES="$CIH_CACHE_MAX_BYTES" \
  target/release/cih-server &
CIH_ACCEPT_SERVER_PID=$!
```

Wait for `/ready`, but do not send any search request. If the single-repository
`platform` validator used this process, restart it again: the soak requires
empty search counters and no retained indexes.

```bash
CIH_API_TOKEN='<token-if-configured>' \
python3 scripts/validate-retrieval-production-soak.py \
  --server-url http://127.0.0.1:8080 \
  --server-pid "$CIH_ACCEPT_SERVER_PID" \
  --manifest /run/secrets/cih-production-soak.json \
  --duration-secs 1800 \
  --warmup-secs 300 \
  --sample-interval-secs 5 \
  --output docs/perf/retrieval-production-soak.json
```

Omit `CIH_API_TOKEN` for an intentionally unauthenticated server. For a
container, execute the runner inside that container or its PID namespace; a
container PID passed from the host (or a host PID passed from the container)
is invalid. The server process environment is the source of truth for cache
limits.

The runner deliberately fails when RSS is unavailable, the server restarts,
cumulative counters decrease, fewer than 90% of memory/health samples arrive,
or any repository reloads/builds/evicts after warm-up. It also requires every
repository to exercise `search_code`, `architecture_overview`, and `grep_files`,
checks tool latency and stable search-result hashes, and evaluates 60-second RSS
medians after the five-minute allocator warm-up. Reports contain hashed labels
and bounded metrics only; selectors, paths, queries, grep patterns, and tool
result bodies are omitted.
