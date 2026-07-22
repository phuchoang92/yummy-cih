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

After this passes, run the scheduled 30-minute mixed soak and the alternating
test against eight distinct production repositories. Those are the final
rollout gates before removing the sidecar rollback switch.
