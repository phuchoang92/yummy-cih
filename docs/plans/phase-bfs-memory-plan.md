# Plan: Fix Memory-Intensive BFS Process Tracing

## Summary

Replace the process tracing BFS queue in `crates/cih-community/src/bfs.rs` with a
parent-pointer trace arena. The goal is to avoid cloning a full `Vec<NodeIndex>` and
`HashSet<NodeIndex>` for every active frontier branch while preserving deterministic
trace output and the existing `Vec<Vec<NodeIndex>>` return type.

## Key Changes

- Queue only arena indexes (`usize`) instead of `(NodeIndex, Vec<NodeIndex>, HashSet<NodeIndex>)`.
- Store path state in:

  ```rust
  struct TraceState {
      node: NodeIndex,
      parent: Option<usize>,
      depth: usize,
  }
  ```

- Detect cycles by walking parent pointers. This is bounded by `max_trace_depth`, which
  defaults to 10.
- Reconstruct `Vec<NodeIndex>` only when a terminal trace is accepted.
- Add `ProcessConfig::max_states_per_entry` with default `50_000`.
- When the per-entry state cap is reached, stop expanding new child states and treat
  already queued states as terminal candidates.

## Implementation Notes

- Keep `trace_process_paths(...) -> Vec<Vec<NodeIndex>>` unchanged.
- Preserve deterministic callee sorting, `max_branching`, `max_trace_depth`,
  `min_steps`, `max_processes`, deduplication, and final sort behavior.
- Add helpers:
  - `contains_ancestor(states, state_idx, next)`
  - `reconstruct_path(states, leaf_idx)`
- Keep this change internal to `cih-community`; no artifact schema changes are needed.

## Test Plan

- Add/adjust `cih-community` tests for:
  - cycle prevention;
  - max branching;
  - max trace depth;
  - max states per entry;
  - deterministic output;
  - existing deduplication behavior.
- Run:

  ```bash
  cargo test -p cih-community
  cargo test --workspace
  ```

## Assumptions

- Parent-pointer ancestor scanning is cheaper than per-state `HashSet` cloning because
  `max_trace_depth` is intentionally small.
- `50_000` states per entry point is a conservative starting cap for banking-scale
  codebases and can be tuned after real repository runs.

