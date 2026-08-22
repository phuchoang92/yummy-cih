# Bounded graph projection benchmark

The graph explorer never materializes the repository-sized graph in the
browser. Its acceptance harness covers three distinct limits:

- deterministic synthetic fixtures at 500,000 and 1,000,000 nodes;
- a real C-heavy legacy codebase, Torvalds Linux v7.2 at commit
  `237a1c39e8dfd3e1c6f1f023eea37a48ec04cc63`;
- the browser-visible projection, capped at 10,000 nodes, 50,000 edges and a
  1 MiB logical JSON response.

Run one scenario or the complete suite:

```bash
scripts/benchmark-graph-projection.sh synthetic-500k
scripts/benchmark-graph-projection.sh synthetic-1m
scripts/benchmark-graph-projection.sh linux
scripts/benchmark-graph-projection.sh all
```

Outputs are written under `target/cih-projection-bench/results`. The synthetic
runs reuse the production scale harness and enforce its cross-platform latency,
event-loop, cache and paging gates. The Linux run builds the portable binary,
indexes the pinned source with an isolated `CIH_HOME`, starts the real embedded
graph server, measures repository projection TTFB/total time, and fails if the
wire response crosses any hard bound.

For UI capacity testing, `graph-ui/e2e/overview.spec.ts` feeds the Canvas
renderer its maximum visible 10k/50k projection. The performance contract is:
the seeded first frame is available before D3 refinement, layout runs in a
Worker, refinement posts at most every 50 ms, targets 300 ms, and stops by
500 ms. While a graph above 5,000 nodes is moving, edges are temporarily
suppressed to keep interaction responsive.
