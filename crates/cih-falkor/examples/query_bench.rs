//! Read-path latency benchmark for the FalkorDB-backed MCP tools.
//!
//! Requires a running FalkorDB with a graph already loaded (e.g. fineract).
//! Prints per-tool median / p95 latency as a JSON line so results are diffable
//! across changes — the committed baseline lives in `docs/perf/read-path-baseline.json`.
//!
//!   FALKOR_URL=redis://127.0.0.1:6380 \
//!   CIH_BENCH_GRAPH=fineract CIH_BENCH_NAME=save \
//!   cargo run -p cih-falkor --example query_bench
//!
//! `CIH_BENCH_NAME` should be a symbol name that exists in the graph; the bench
//! resolves it to a node id for the context/impact measurements.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cih_falkor::FalkorStore;
use cih_graph_store::{Direction, GraphStore};

const ITERS: usize = 50;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::var("FALKOR_URL").unwrap_or_else(|_| "redis://127.0.0.1:6380".into());
    let graph = std::env::var("CIH_BENCH_GRAPH").unwrap_or_else(|_| "fineract".into());
    let name = std::env::var("CIH_BENCH_NAME").unwrap_or_else(|_| "save".into());
    eprintln!("FalkorDB: {url}  graph: {graph}  name: {name}  iters: {ITERS}\n");

    let store =
        Arc::new(FalkorStore::connect(&url, &graph)?.with_query_limit(64, Duration::from_secs(5)));
    store.graph_summary().await?; // warm up the connection

    let mut results: Vec<(String, Stats)> = Vec::new();

    // ── name resolution (candidates_by_name) — the n.name index hot path ──────
    results.push((
        "name_lookup".into(),
        bench(ITERS, || async {
            store.candidates_by_name(&name, 50).await.map(|_| ())
        })
        .await?,
    ));

    // Resolve the name to a node id for the id-keyed tools below.
    let id = match store.candidates_by_name(&name, 1).await?.into_iter().next() {
        Some(node) => node.id,
        None => {
            eprintln!("no symbol named {name:?} in graph {graph:?}; set CIH_BENCH_NAME");
            print_json(&results);
            return Ok(());
        }
    };
    eprintln!("resolved id: {}\n", id.as_str());

    // ── context (concurrent try_join of 4 sub-queries) ───────────────────────
    results.push((
        "context".into(),
        bench(ITERS, || async { store.context(&id).await.map(|_| ()) }).await?,
    ));

    // ── context_sequential: the same four sub-queries awaited one-by-one, i.e.
    //    the pre-concurrency shape. The gap vs `context` is the item-5 win. ────
    results.push((
        "context_sequential".into(),
        bench(ITERS, || async {
            store.neighbors(&id, Direction::Upstream, &[]).await?;
            store.neighbors(&id, Direction::Downstream, &[]).await?;
            store
                .processes_for_symbols(std::slice::from_ref(&id))
                .await?;
            store.symbol_communities(std::slice::from_ref(&id)).await?;
            Ok(())
        })
        .await?,
    ));

    // ── impact (variable-length upstream traversal, depth 4) ──────────────────
    results.push((
        "impact_upstream_d4".into(),
        bench(ITERS, || async {
            store.impact(&id, Direction::Upstream, 4).await.map(|_| ())
        })
        .await?,
    ));

    // ── route_map (kind-indexed + path prefix scan) ───────────────────────────
    results.push((
        "route_map".into(),
        bench(ITERS, || async {
            store.route_map(None, 200).await.map(|_| ())
        })
        .await?,
    ));

    // ── detect_changes fan-out: N per-node impact traversals, serial vs. the
    //    concurrent JoinSet shape. Gather real ids from one impact's blast radius.
    let seed = store.impact(&id, Direction::Upstream, 4).await?;
    let ids: Vec<_> = seed
        .affected
        .iter()
        .take(20)
        .map(|n| n.id.clone())
        .collect();
    if ids.len() >= 2 {
        let n = ids.len();
        const FANOUT_ITERS: usize = 10;
        results.push((
            format!("detect_changes_seq_{n}nodes"),
            bench(FANOUT_ITERS, || async {
                for nid in &ids {
                    store.impact(nid, Direction::Upstream, 4).await?;
                }
                Ok(())
            })
            .await?,
        ));
        results.push((
            format!("detect_changes_concurrent_{n}nodes"),
            bench(FANOUT_ITERS, || async {
                let mut set = tokio::task::JoinSet::new();
                for nid in &ids {
                    let store = store.clone();
                    let nid = nid.clone();
                    set.spawn(async move { store.impact(&nid, Direction::Upstream, 4).await });
                }
                while let Some(joined) = set.join_next().await {
                    let _ = joined;
                }
                Ok(())
            })
            .await?,
        ));
    }

    print_json(&results);
    Ok(())
}

struct Stats {
    median_ms: f64,
    p95_ms: f64,
    min_ms: f64,
    max_ms: f64,
}

/// Run `op` `iters` times, returning latency percentiles.
async fn bench<F, Fut>(iters: usize, mut op: F) -> anyhow::Result<Stats>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = cih_graph_store::Result<()>>,
{
    let mut samples: Vec<f64> = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t0 = Instant::now();
        op().await?;
        samples.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    let pct = |p: f64| {
        samples[((p * (samples.len() as f64 - 1.0)).round() as usize).min(samples.len() - 1)]
    };
    Ok(Stats {
        median_ms: pct(0.5),
        p95_ms: pct(0.95),
        min_ms: samples[0],
        max_ms: samples[samples.len() - 1],
    })
}

fn print_json(results: &[(String, Stats)]) {
    let tools: Vec<String> = results
        .iter()
        .map(|(name, s)| {
            format!(
                "    {{\"tool\": {name:?}, \"median_ms\": {:.3}, \"p95_ms\": {:.3}, \"min_ms\": {:.3}, \"max_ms\": {:.3}}}",
                s.median_ms, s.p95_ms, s.min_ms, s.max_ms
            )
        })
        .collect();
    println!(
        "{{\n  \"iters\": {ITERS},\n  \"tools\": [\n{}\n  ]\n}}",
        tools.join(",\n")
    );
}
