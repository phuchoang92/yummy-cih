# 500k-node scale reference run

Scale measurement record for `docs/plans/cih-server-clean-architecture-and-scalability.md`
and `docs/plans/search-index-scale-performance.md`.

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

Reference machine for the latest numbers below: macOS / aarch64, 14 logical
CPUs, release profile, 2026-07-22. The fixture was reused and the run enforced
all acceptance gates.

## Results

| Scenario | Observed | Acceptance target | Verdict |
|---|---:|---|---|
| Artifact cold parse (500k nodes + 1M edges) | 541 ms | — | baseline |
| Artifact lazy index build | 181 ms | — | built on first indexed access |
| Compact BM25 build | 863 ms | — | 500,000 documents |
| Search sidecar persist | 476 ms | — | 128.6 MiB payload |
| Search sidecar warm-restart load | 426 ms | < 10,000 ms | **pass** |
| Artifact cache hit p95 | 0.037 ms | < 5 ms | **pass** |
| Event-loop delay p99 during cold load | 1.263 ms | < 50 ms | **pass** |
| Same-key artifact cold burst (16 callers) | 1 loader build, shared snapshot | exactly 1 | **pass** |
| Oversize search cold burst (16 callers) | 1 sidecar load, 0 retained bytes | exactly 1 load/build | **pass** |
| Corrupt sidecar recovery + restart | 1 fallback, 1 repair, 1 restart load | all three | **pass** |
| Eight-repository alternating hot set | 8 retained, 0 second-pass loads/evictions, 4.017 ms p95 | declared budget | **pass** |
| Resource tail page p95 | 0.048 ms | < 5 ms | **pass** |
| BM25 search query p95 (500k docs) | 3.734 ms | < 500 ms | **pass** |

Estimated retained memory at 500k nodes is 761.8 MiB for the artifact snapshot,
190.3 MiB for the compact BM25 index, and 8.7 MiB for the synthetic wiki index.
The macOS RSS probe returned no sample in this run, so this report does not claim
an observed process peak.

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

1. **BM25 latency and retained size are closed for this milestone.** Dense score
   accumulation, bounded top-k selection, file interning, and removal of retained
   source text reduced the measured index from the prior ≈ 362 MiB to 190.3 MiB.
   Ranking-equivalence tests pass and query p95 is 3.734 ms. A 500k-document
   index now fits the default 256 MiB search-cache byte budget.
2. **Cold search reuse and recovery are closed.** The sidecar loaded in 426 ms.
   With the cache deliberately set to one byte, 16 concurrent callers shared one
   load and retained nothing. Corrupting the sidecar caused one streaming
   fallback build, one atomic repair, and a successful sidecar load after a
   simulated restart.
3. **Aggregate hot-set sizing is enforced by the harness.** Eight cloned
   500k-document repositories retained 1,596,642,224 bytes under a declared
   1,756,306,446-byte budget. A second alternating pass caused no sidecar loads
   or evictions and measured 4.017 ms p95. The equivalent test on eight real
   repositories remains a production rollout gate because their indexes will
   have different sizes and term distributions.
4. **The artifact snapshot remains oversize for its default cache.** Its
   estimated 761.8 MiB exceeds `CIH_ARTIFACT_CACHE_MAX_BYTES` (512 MiB), so it is
   served without retention unless operators raise that budget. Compact or
   memory-mapped graph artifacts remain a separate future representation change.
5. **Observed peak RSS remains open on this machine.** The harness emitted
   retained-size estimates but the macOS process RSS probe returned `null`.
   Capacity decisions that require process-peak evidence need an RSS-capable
   environment or a corrected macOS sampler.
