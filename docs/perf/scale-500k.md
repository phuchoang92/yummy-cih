# 500k-node scale reference run

Milestone 5 measurement record for `docs/plans/cih-server-clean-architecture-and-scalability.md`
(§21.5 scale tests, §21.6 acceptance targets).

## Method

```bash
cargo run --release -p cih-server --example scale_bench -- \
  --nodes 500000 --edges-per-node 2 --iterations 20 \
  --burst-callers 16 --search-cache-bytes 1 \
  --output docs/perf/scale-500k-local.json [--enforce]
```

The harness (`crates/cih-server/src/scale_bench.rs`) generates a deterministic
fixture — 500,000 `Method` nodes (179 MB `nodes.jsonl`), 1,000,000 `Calls` edges
(164 MB `edges.jsonl`), 50,000 `Community` records — then drives the **production**
adapters: `ArtifactCache` snapshot/index loading, the BM25 `SearchIndex`, the wiki
`TextIndex`, the search sidecar codec, and the MCP resource paging scan. The
report includes a field-by-field retained-memory breakdown plus sidecar persist
and warm-restart load durations. `scale-500k-local.json` holds the
latest raw report; it is machine-specific and regenerated, not a golden file.

The result table below predates the compact search representation introduced by
`docs/plans/search-index-scale-performance.md`; do not use its 362 MiB BM25
figure as the current value. Regenerate the raw report before making a capacity
decision. The harness schema now records the replacement measurements.

Reference machine for the latest numbers below: macOS / aarch64, 14 logical
CPUs, release profile, 2026-07-21.

## Results

| Scenario | Observed | §21.6 target | Verdict |
|---|---:|---|---|
| Artifact cold parse (500k nodes + 1M edges) | 752-817 ms | — | baseline |
| First persisted positional index build | 718 ms | — | writes sidecar |
| Warm-restart persisted index load | 199 ms | — | **3.6x faster** |
| Artifact cache hit p95 | 0.027-0.032 ms | — | fast path is an `Arc` clone |
| Event-loop delay p99 during cold load | 1.27 ms | < 50 ms | **pass** |
| Same-key cold burst (8 callers) | 1 loader build, shared snapshot | exactly 1 | **pass** |
| Resource tail page p95 | 0.039 ms | < 2000 ms | **pass** |
| BM25 search query p95 (500k docs) | 16.2-17.1 ms | < 500 ms | **pass** |

Memory at 500k nodes: artifact snapshot ≈ 762 MiB estimated, BM25 index ≈ 362 MiB,
peak RSS ≈ 2.0–2.9 GiB across the full harness (which holds artifact, index, and
search structures live simultaneously — the server never does).

## Resource paging: before / after

The M5 work item "avoid full scans for resource pages" is implemented by
`infrastructure/jsonl_page_index.rs`: a cached byte-offset index per
(file, kind) so a page seeks to its first record instead of re-parsing every
earlier one.

| Page | Before (p50 / p95) | After (p50 / p95) | p95 speedup |
|---|---|---|---:|
| first | 0.042 / 0.054 ms | 0.042 / 0.053 ms | 1× |
| middle (offset 25,000) | 7.758 / 7.963 ms | 0.039 / 0.040 ms | 198× |
| tail (offset 49,900) | 15.387 / 16.520 ms | 0.038 / 0.039 ms | 426× |

Cost is now **flat in offset** rather than linear, so walking the whole resource
went from quadratic (~4 s of pure rescanning for 500 pages) to linear. The
`resource_tail_page_p95` acceptance threshold was tightened from 2000 ms to 5 ms
so a regression to full-scan paging fails the benchmark.

## Closed and open findings

1. **BM25 latency is closed.** Dense score accumulation plus linear top-k
   selection replaced a 500k-entry hash map and full candidate sort. Ranking
   equivalence tests pass; two release runs measured 16.216 ms and 17.132 ms
   p95, roughly 29-31x below the previous 489-511 ms range.
2. **Default cache budgets are smaller than a single 500k-node repository.** The
   artifact snapshot alone (≈ 762 MiB) exceeds `CIH_ARTIFACT_CACHE_MAX_BYTES`
   (512 MiB) and the BM25 index (≈ 362 MiB) exceeds
   `CIH_SEARCH_CACHE_MAX_BYTES` (256 MiB), so at this scale both are served
   without being retained (the oversize bypass). Correct and bounded, but it
   means no caching benefit for a repository this large until the budgets are
   raised or the representations shrink.
3. **Persisted adjacency reuse is implemented, but memory size is not closed.**
   The checksummed sidecar reduced warm adjacency initialization from 718 ms to
   199 ms. Nodes, edges, and loaded hash indexes remain resident and the
   estimated snapshot is about 762 MiB; memory mapping/compact ordinals remain
   a future representation change rather than a claimed result of persistence.
