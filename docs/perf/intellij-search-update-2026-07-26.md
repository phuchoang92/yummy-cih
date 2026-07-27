# IntelliJ search update comparison — 2026-07-26

Status: local comparative evidence; not a production qualification report.

## Decision

The large-repository update does **not materially change lexical BM25 latency**
on this IntelliJ fixture. Median process-cold and warm p95 differences are below
one percent, and every ordered hit digest is identical. This is expected because
the search index and ranking algorithm did not change.

The update materially improves expanded-search containment and correctness:
the high-fan `ApplicationManager#getApplication` response is 51 percent smaller
at the new default budget, returns no dangling edges, and has essentially the
same p95. Expansion is not uniformly faster: `Disposer#register` is slower at
the new default because the server now evaluates and returns 500 endpoint-closed
nodes instead of the old fixed 201-node slice.

The cache-default increase from 256 MiB to 512 MiB is not exercised by this
fixture. Its decoded index is 144,426,313 bytes and therefore fits both versions.
The observed production index of approximately 409 MiB should stop reloading
under the new default, but that claim still requires a run with that actual
artifact.

## Revisions and fixture

| Item | Value |
|---|---|
| Baseline CIH | `b1420829810f1f4f6533620408006fbab1098963` |
| Updated CIH | `277a9dd9224478d8710207efaf33d76cd1c22706` |
| IntelliJ | `f0b8096f352ed37bacfc8a3fcf10e2df3fb916b0` |
| Scope | `intellij-community/platform` |
| Artifact version | `7751ae60f077c7db` |
| Graph | 415,604 nodes; 941,559 edges |
| Search sidecar | 120,409,953 bytes on disk; 144,426,313 retained bytes |
| Sidecar SHA-256 | `b64b5fa15b05ae978bc906d2b3491b065b3f1006da2cacfd060c990c22aa500e` |
| Rust | `rustc 1.97.1`, aarch64 macOS |

Both revisions were built from exact Git archives in release mode. Their binary
SHA-256 values were:

- baseline: `47c1f3d4d20b1e31f1bcde25ce95d0cd2e9aebe8841f0055c394fa20f6416b9f`;
- updated: `ac0356c1fc6a26a2dd0ba621556a048010b90d175a237fee78824f9aee0e05ba`.

## Isolation and protocol

The original IntelliJ `.cih` directory was not used as a writable artifact
root. Each revision received a separate temporary facade containing read-only
symlinks to `nodes.jsonl` and `edges.jsonl` plus its own copied
`search-index.bin`. The original and both copies retained the same sidecar hash
after all runs. No analysis, migration, reindex, repair, or pruning ran.

Both revisions used the same live read-only graph and explicit serving limits:

```text
CIH_SEARCH_CACHE_MAX_ENTRIES=1
CIH_SEARCH_CACHE_MAX_BYTES=268435456
CIH_SEARCH_COLD_MAX_CONCURRENT=1
CIH_SEARCH_COLD_MAX_BYTES=536870912
CIH_CACHE_MAX_BYTES=1610612736
semantic search disabled
```

Requests used `GET /api/graph/search`, which drives the same production
`query_hits` and graph-expansion services without resolving the legacy global
registry. Consequently these timings include HTTP JSON serialization but not
MCP's JSON-RPC envelope, compatibility duplication, or response-guard counting.
An end-to-end MCP production run remains a separate transport gate.

Run order alternated `baseline, updated, updated, baseline, baseline, updated`.
Each revision used three fresh server processes. Each process ran:

- one 16-client process-cold burst for `ApplicationManager getApplication`;
- 30 timed warm samples for each of seven lexical queries after warmup;
- 20 expanded samples for `ApplicationManager getApplication` and
  `Disposer register`;
- continuous `/health` polling during the cold burst;
- cache, sidecar, flight, scorer, and semantic counter capture.

The OS page cache was not reset, so “cold” means a fresh CIH process/index cache,
not a guaranteed storage-cold disk. The alternating order reduces, but does not
eliminate, machine noise.

## Lexical search results

Values are medians of the three independent process runs.

| Measurement | Baseline | Updated | Delta | Interpretation |
|---|---:|---:|---:|---|
| 16-client cold p95 | 317.441 ms | 315.050 ms | -0.75% | no meaningful change |
| 16-client cold wall time | 321.268 ms | 318.768 ms | -0.78% | no meaningful change |
| Worst warm-query p95 | 1.6309 ms | 1.6308 ms | -0.01% | equivalent |
| Cold `/health` p99 | 0.673 ms | 0.680 ms | +1.0% | equivalent; far below 50 ms |
| Sidecar loads per process | 1 | 1 | — | expected |
| Joined cold waiters | 15 | 15 | — | single-flight works |
| Fallback builds / repairs | 0 / 0 | 0 / 0 | — | valid sidecar used |
| Retained index entries | 1 | 1 | — | warm contract met |
| Semantic attempts | 0 | 0 | — | lexical lane remained isolated |

All seven ordered lexical result digests matched across every baseline and
updated run. The update therefore introduced no observed ranking or result drift.

Raw per-process headline values:

| Run | Cold p95 | Cold wall | Worst warm p95 |
|---|---:|---:|---:|
| baseline 1 | 339.181 ms | 358.003 ms | 1.841 ms |
| baseline 2 | 310.964 ms | 314.061 ms | 1.631 ms |
| baseline 3 | 317.441 ms | 321.268 ms | 1.509 ms |
| updated 1 | 326.855 ms | 330.214 ms | 1.632 ms |
| updated 2 | 314.853 ms | 318.768 ms | 1.631 ms |
| updated 3 | 315.050 ms | 318.168 ms | 1.592 ms |

## Expanded search results

Values are medians of three processes using the updated default expansion limits
of 500 nodes, 1,000 edges, and 262,144 logical response bytes. The baseline
accepts but ignores those new limit parameters.

| Query and measurement | Baseline | Updated | Result |
|---|---:|---:|---|
| `ApplicationManager`: p95 | 17.915 ms | 18.055 ms | equivalent latency |
| `ApplicationManager`: body | 535,117 B | 261,890 B | 51.1% smaller |
| `ApplicationManager`: nodes / edges | 201 / 2,142 | 500 / 344 | more closed nodes, bounded edges |
| `ApplicationManager`: dangling edges | 1,942 | 0 | correctness fixed |
| `Disposer`: p95 | 5.928 ms | 10.201 ms | 72.1% slower |
| `Disposer`: body | 174,595 B | 261,806 B | 49.9% larger |
| `Disposer`: nodes / edges | 201 / 532 | 500 / 221 | 2.5x nodes, endpoint-closed edges |
| `Disposer`: dangling edges | 332 | 0 | correctness fixed |

The updated response explicitly reports `complete=false`, traversal
`evaluation_status=inconclusive`, returned/omitted counts, visited/expanded work,
and reasons such as `response_budget`, `node_budget`, and `edge_budget`. The
baseline response provides no equivalent completeness evidence.

One additional updated run used a 200-node cap to approximate the old fixed
node count:

| Query | Updated p95 | Body | Nodes / edges | Dangling |
|---|---:|---:|---:|---:|
| `ApplicationManager` | 15.237 ms | 115,443 B | 200 / 199 | 0 |
| `Disposer` | 8.808 ms | 141,851 B | 200 / 199 | 0 |

At the comparable node budget, `ApplicationManager` is faster and 78 percent
smaller than baseline. `Disposer` remains slower, showing that shared traversal
and endpoint-closure work has a measurable cost even when the output is smaller.

## Conclusion and next gate

The update should be described as **latency-neutral for lexical search and much
safer for expanded search**, not as a general search speedup. Users who need the
lowest expanded-search latency can request a smaller node/edge/byte budget and
receive explicit incompleteness instead of a misleading graph.

The next decisive performance test is the actual approximately 409 MiB decoded
index that previously exceeded the 256 MiB cache. Run the same alternating
process protocol there and require one initial load, retained state, and zero
later loads. That test can confirm whether the new 512 MiB default removes the
observed 23-second repeated cold-search behavior.
