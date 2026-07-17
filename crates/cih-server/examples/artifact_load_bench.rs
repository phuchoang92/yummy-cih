//! Cost of parsing `nodes.jsonl` + `edges.jsonl` — the per-call load that
//! `taint_paths` and `shape_check` used to pay on *every* invocation and that the
//! `ArtifactCache` now serves from memory (an Arc clone) after the first call.
//!
//!   CIH_BENCH_ARTIFACTS=/abs/path/.cih/artifacts/<hash> \
//!   cargo run -p cih-server --example artifact_load_bench

use std::time::Instant;

use cih_server::utils::{load_artifact_edges, load_artifact_nodes};

fn main() -> anyhow::Result<()> {
    let dir = std::env::var("CIH_BENCH_ARTIFACTS")
        .expect("set CIH_BENCH_ARTIFACTS to an artifacts dir containing nodes.jsonl/edges.jsonl");

    // A few cold parses so the number is not a single-sample fluke; the OS page
    // cache warms after the first, isolating parse (CPU) from disk I/O.
    for i in 0..3 {
        let t0 = Instant::now();
        let nodes = load_artifact_nodes(&dir)?;
        let nodes_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = Instant::now();
        let edges = load_artifact_edges(&dir)?;
        let edges_ms = t1.elapsed().as_secs_f64() * 1000.0;
        println!(
            "run {i}: nodes={} ({:.1} ms)  edges={} ({:.1} ms)  total={:.1} ms  (cache hit avoids all of this)",
            nodes.len(),
            nodes_ms,
            edges.len(),
            edges_ms,
            nodes_ms + edges_ms
        );
    }
    Ok(())
}
