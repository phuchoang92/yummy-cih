//! Live load test for FalkorStore pooling + backpressure.
//!
//! Requires a running FalkorDB. Uses a throwaway graph key and deletes it after.
//!
//!   FALKOR_URL=redis://127.0.0.1:6380 cargo run -p cih-falkor --example load_test
//!
//! While it runs, sample connection count from another shell to see reuse:
//!   watch -n0.2 'redis-cli -p 6380 INFO clients | grep connected_clients'

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cih_falkor::FalkorStore;
use cih_graph_store::{GraphStore, GraphStoreError};

const KEY: &str = "__cih_loadtest__";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::var("FALKOR_URL").unwrap_or_else(|_| "redis://127.0.0.1:6380".into());
    println!("FalkorDB: {url}  graph: {KEY}\n");

    // ---- Phase 1: connection reuse under sustained concurrent load --------
    // Generous limit so the semaphore never sheds; the point is that all this
    // concurrency runs over ONE reused connection, not one-per-request.
    let store =
        Arc::new(FalkorStore::connect(&url, KEY)?.with_query_limit(64, Duration::from_secs(5)));
    store.graph_summary().await?; // warm up / create the graph

    let workers = 40usize;
    let per_worker = 500usize;
    println!(
        "Phase 1 (reuse): {} workers x {} queries = {} total, limit=64",
        workers,
        per_worker,
        workers * per_worker
    );
    let t0 = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..workers {
        let s = store.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..per_worker {
                s.graph_summary().await.expect("summary ok");
            }
        }));
    }
    for h in handles {
        h.await?;
    }
    let elapsed = t0.elapsed();
    let total = (workers * per_worker) as f64;
    println!(
        "  done in {:?}  ({:.0} queries/s)",
        elapsed,
        total / elapsed.as_secs_f64()
    );
    println!(
        "  -> check connected_clients now stays ~1-2 (reuse), not ~{} (one per in-flight)\n",
        workers
    );

    // ---- Phase 2: backpressure sheds under a tight limit -----------------
    let tight =
        Arc::new(FalkorStore::connect(&url, KEY)?.with_query_limit(2, Duration::from_millis(20)));
    let burst = 200usize;
    println!("Phase 2 (backpressure): {burst} concurrent queries, limit=2, acquire_timeout=20ms");
    let (ok, shed) = run_burst(&tight, burst).await;
    println!("  ok={ok}  shed(overloaded)={shed}");
    assert!(shed > 0, "expected some requests to shed under limit=2");

    // ---- Phase 3: no false shedding when capacity is ample ---------------
    let ample =
        Arc::new(FalkorStore::connect(&url, KEY)?.with_query_limit(64, Duration::from_millis(20)));
    println!("Phase 3 (headroom): {burst} concurrent queries, limit=64, acquire_timeout=20ms");
    let (ok, shed) = run_burst(&ample, burst).await;
    println!("  ok={ok}  shed(overloaded)={shed}");
    assert_eq!(shed, 0, "no request should shed with limit=64");

    // ---- Cleanup ----------------------------------------------------------
    store.drop_graph().await?;
    println!("\ncleaned up graph '{KEY}'.  PASS");
    Ok(())
}

/// Fire `n` concurrent `graph_summary` calls; return (ok, shed) counts.
async fn run_burst(store: &Arc<FalkorStore>, n: usize) -> (u64, u64) {
    let ok = Arc::new(AtomicU64::new(0));
    let shed = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for _ in 0..n {
        let (s, ok, shed) = (store.clone(), ok.clone(), shed.clone());
        handles.push(tokio::spawn(async move {
            match s.graph_summary().await {
                Ok(_) => {
                    ok.fetch_add(1, Ordering::Relaxed);
                }
                Err(GraphStoreError::Backend(m)) if m.contains("overloaded") => {
                    shed.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        }));
    }
    for h in handles {
        h.await.expect("task join");
    }
    (ok.load(Ordering::Relaxed), shed.load(Ordering::Relaxed))
}
