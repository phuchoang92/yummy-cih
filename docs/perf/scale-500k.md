# 500k-node scale reference run

Milestone 5 measurement record for `docs/plans/cih-server-clean-architecture-and-scalability.md`
(§21.5 scale tests, §21.6 acceptance targets).

## Method

```bash
cargo run --release -p cih-server --example scale_bench -- \
  --nodes 500000 --edges-per-node 2 --iterations 20 \
  --output docs/perf/scale-500k-local.json [--enforce]
```

The harness (`crates/cih-server/src/scale_bench.rs`) generates a deterministic
fixture — 500,000 `Method` nodes (179 MB `nodes.jsonl`), 1,000,000 `Calls` edges
(164 MB `edges.jsonl`), 50,000 `Community` records — then drives the **production**
adapters: `ArtifactCache` snapshot/index loading, the BM25 `SearchIndex`, the wiki
`TextIndex`, and the MCP resource paging scan. `scale-500k-local.json` holds the
latest raw report; it is machine-specific and regenerated, not a golden file.

Reference machine for the numbers below: macOS / aarch64, 14 logical CPUs,
release profile, 2026-07-20.

## Results

| Scenario | Observed | §21.6 target | Verdict |
|---|---:|---|---|
| Artifact cold parse (500k nodes + 1M edges) | 706 ms | — | baseline |
| Lazy positional index build | 276 ms | — | baseline |
| Artifact cache hit p95 | 0.029 ms | — | fast path is an `Arc` clone |
| Event-loop delay p99 during cold load | 0.65 ms | < 50 ms | **pass** |
| Same-key cold burst (8 callers) | 1 loader build, shared snapshot | exactly 1 | **pass** |
| Resource tail page p95 | 0.039 ms | < 2000 ms | **pass** |
| BM25 search query p95 (500k docs) | 489–511 ms | < 500 ms | **at/over the line** |

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

## Open findings

1. **BM25 search at 500k documents sits on the §21.6 500 ms p95 target**
   (489 ms and 511 ms across two runs; p99 ≈ 512 ms). It is not a regression from
   the paging work — search is untouched by it — but it is the next real
   scalability item: either the query path needs optimizing or the target needs
   an explicit revision for repositories of this size.
2. **Default cache budgets are smaller than a single 500k-node repository.** The
   artifact snapshot alone (≈ 762 MiB) exceeds `CIH_ARTIFACT_CACHE_MAX_BYTES`
   (512 MiB) and the BM25 index (≈ 362 MiB) exceeds
   `CIH_SEARCH_CACHE_MAX_BYTES` (256 MiB), so at this scale both are served
   without being retained (the oversize bypass). Correct and bounded, but it
   means no caching benefit for a repository this large until the budgets are
   raised or the representations shrink.
