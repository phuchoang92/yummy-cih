//! Deadlines for the server's blocking loads. Every CPU-/IO-heavy operation runs
//! on the Tokio blocking pool via `spawn_blocking`; [`run_blocking`] wraps that
//! with a timeout so a wedged load (corrupt artifact, pathological regex, a stuck
//! read) returns a typed error instead of hanging up to the 120 s HTTP
//! `TimeoutLayer`.
//!
//! Note: `spawn_blocking` tasks are **uncancellable** — on timeout the closure
//! still runs to completion on the pool; the caller merely stops waiting. That
//! is the only possible behavior without cooperative cancellation, and it is
//! still the win: a prompt typed error vs a two-minute hang.

use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rmcp::ErrorData as McpError;
use tokio::sync::Semaphore;

/// Failure of a deadline-guarded blocking task.
#[derive(Debug)]
pub(crate) enum BlockingError {
    /// The task exceeded its deadline (and, per the module note, is still running).
    TimedOut { label: &'static str, secs: u64 },
    /// The task panicked (surfaced as a `JoinError`).
    Panicked { label: &'static str, detail: String },
    /// The heavy lane stayed saturated past the queue timeout — the task was
    /// rejected before doing any work.
    Saturated {
        label: &'static str,
        waited_secs: u64,
    },
}

impl fmt::Display for BlockingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlockingError::TimedOut { label, secs } => {
                write!(f, "{label} timed out after {secs}s")
            }
            BlockingError::Panicked { label, detail } => {
                write!(f, "{label} task panicked: {detail}")
            }
            BlockingError::Saturated { label, waited_secs } => {
                write!(
                    f,
                    "{label} rejected: heavy blocking lane saturated (waited {waited_secs}s) — \
                     retry shortly"
                )
            }
        }
    }
}

impl std::error::Error for BlockingError {}

// Lets the McpError tool handlers use `?` directly; `anyhow` sites get `?` for
// free via the `std::error::Error` impl above.
impl From<BlockingError> for McpError {
    fn from(err: BlockingError) -> Self {
        McpError::internal_error(err.to_string(), None)
    }
}

/// Run `f` on the blocking pool with a deadline. Returns the value, or
/// [`BlockingError`] on timeout or panic. On timeout the underlying task is
/// **not** cancelled (see the module note) — the caller just stops waiting.
pub(crate) async fn run_blocking<T, F>(
    timeout: Duration,
    label: &'static str,
    f: F,
) -> Result<T, BlockingError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    match tokio::time::timeout(timeout, tokio::task::spawn_blocking(f)).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(join)) => Err(BlockingError::Panicked {
            label,
            detail: join.to_string(),
        }),
        Err(_elapsed) => Err(BlockingError::TimedOut {
            label,
            secs: timeout.as_secs(),
        }),
    }
}

/// Deadline for blocking loads, read once from `CIH_BLOCKING_TIMEOUT_SECS`
/// (default 90 s — comfortably under the 120 s HTTP `TimeoutLayer`, so callers
/// get this typed error first). A value of `0` disables the deadline. Mirrors
/// the read-once pattern of `app::tool_timing_enabled`.
pub(crate) fn blocking_timeout() -> Duration {
    static TIMEOUT: OnceLock<Duration> = OnceLock::new();
    *TIMEOUT.get_or_init(|| {
        let secs = std::env::var("CIH_BLOCKING_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(90);
        if secs == 0 {
            // Disabled: an effectively-infinite (but Tokio-timer-safe) deadline.
            Duration::from_secs(365 * 24 * 60 * 60)
        } else {
            Duration::from_secs(secs)
        }
    })
}

/// The *heavy* blocking lane: a semaphore bounding concurrent cold artifact
/// parses (cross-repo contracts, resource scans, taint loads), read once from
/// `CIH_BLOCKING_MAX_CONCURRENT` (unset/invalid/0 = 2). Light blocking work
/// (grep, wiki page reads) keeps using [`run_blocking`] unguarded — the lane
/// stops N hundred-MB parses from monopolizing the pool, not interactive
/// tools from running.
fn heavy_lane() -> Arc<Semaphore> {
    static LANE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    LANE.get_or_init(|| {
        let permits = std::env::var("CIH_BLOCKING_MAX_CONCURRENT")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(2);
        Arc::new(Semaphore::new(permits))
    })
    .clone()
}

/// How long a heavy task may wait for a lane slot before being rejected with
/// [`BlockingError::Saturated`]. `CIH_BLOCKING_QUEUE_TIMEOUT_SECS`, default 5;
/// 0 disables the queue timeout.
fn heavy_queue_timeout() -> Duration {
    static TIMEOUT: OnceLock<Duration> = OnceLock::new();
    *TIMEOUT.get_or_init(|| {
        let secs = std::env::var("CIH_BLOCKING_QUEUE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(5);
        if secs == 0 {
            Duration::from_secs(365 * 24 * 60 * 60)
        } else {
            Duration::from_secs(secs)
        }
    })
}

/// [`run_blocking`] behind the heavy lane: waits up to the queue timeout for a
/// slot, then runs with the usual deadline. Use for cold artifact loads.
pub(crate) async fn run_blocking_heavy<T, F>(
    timeout: Duration,
    label: &'static str,
    f: F,
) -> Result<T, BlockingError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    run_gated(heavy_lane(), heavy_queue_timeout(), timeout, label, f).await
}

/// Lane-gated core, taking the semaphore explicitly so tests can use a local
/// lane instead of racing on the process-wide one.
async fn run_gated<T, F>(
    lane: Arc<Semaphore>,
    queue_timeout: Duration,
    deadline: Duration,
    label: &'static str,
    f: F,
) -> Result<T, BlockingError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let permit = match tokio::time::timeout(queue_timeout, lane.acquire_owned()).await {
        Ok(Ok(permit)) => permit,
        // The lane is never closed; treat it like a panic if it ever is.
        Ok(Err(closed)) => {
            return Err(BlockingError::Panicked {
                label,
                detail: closed.to_string(),
            })
        }
        Err(_elapsed) => {
            return Err(BlockingError::Saturated {
                label,
                waited_secs: queue_timeout.as_secs(),
            })
        }
    };
    // The permit rides inside the closure: a timed-out load keeps its slot
    // until the (uncancellable) closure actually finishes, so saturation
    // reflects work running on the pool, not work being awaited (§9.3 of the
    // design record — never start another heavy load while a timed-out one is
    // still running).
    run_blocking(deadline, label, move || {
        let _slot = permit;
        f()
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    #[tokio::test]
    async fn fast_closure_returns_value() {
        let out = run_blocking(Duration::from_secs(5), "fast", || 21 * 2).await;
        assert_eq!(out.unwrap(), 42);
    }

    /// The deadline fires, not the closure: the call returns well before the
    /// 500 ms body finishes, and the body's side effect never lands in time.
    #[tokio::test]
    async fn slow_closure_times_out_promptly() {
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let start = Instant::now();
        let out: Result<(), BlockingError> =
            run_blocking(Duration::from_millis(50), "slow", move || {
                std::thread::sleep(Duration::from_millis(500));
                flag.store(true, Ordering::SeqCst);
            })
            .await;
        assert!(matches!(out, Err(BlockingError::TimedOut { .. })));
        assert!(
            start.elapsed() < Duration::from_millis(400),
            "should return on the 50ms deadline, not wait out the 500ms body"
        );
        assert!(!done.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn panicking_closure_maps_to_panicked() {
        let out: Result<(), BlockingError> =
            run_blocking(Duration::from_secs(5), "boom", || panic!("kaboom")).await;
        assert!(matches!(out, Err(BlockingError::Panicked { .. })));
    }

    /// §9.3 semantics: the lane slot belongs to the *running closure*, not the
    /// awaiting caller — a timed-out-but-still-running load keeps the lane
    /// saturated until it actually finishes.
    #[tokio::test(flavor = "multi_thread")]
    async fn saturated_lane_rejects_within_queue_timeout_until_the_load_finishes() {
        let lane = Arc::new(tokio::sync::Semaphore::new(1));

        // Occupy the single slot with a load that outlives its own deadline.
        let occupant = tokio::spawn(run_gated(
            lane.clone(),
            Duration::from_secs(5),
            Duration::from_millis(50), // deadline fires long before the body ends
            "occupant",
            || std::thread::sleep(Duration::from_millis(500)),
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        // The occupant has timed out for its caller…
        assert!(matches!(
            occupant.await.unwrap(),
            Err(BlockingError::TimedOut { .. })
        ));
        // …but its closure still runs and holds the slot: a newcomer with a
        // short queue timeout is rejected promptly.
        let start = std::time::Instant::now();
        let out = run_gated(
            lane.clone(),
            Duration::from_millis(50),
            Duration::from_secs(5),
            "newcomer",
            || 1,
        )
        .await;
        assert!(
            matches!(out, Err(BlockingError::Saturated { .. })),
            "{out:?}"
        );
        assert!(start.elapsed() < Duration::from_millis(400));

        // Once the occupant's closure completes, the lane frees up.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let out = run_gated(
            lane,
            Duration::from_millis(50),
            Duration::from_secs(5),
            "after",
            || 2,
        )
        .await;
        assert_eq!(out.unwrap(), 2);
    }

    #[test]
    fn saturated_maps_to_mcp_internal_error_with_label() {
        let err = BlockingError::Saturated {
            label: "api_impact artifact load",
            waited_secs: 5,
        };
        let mcp: McpError = err.into();
        let rendered = format!("{mcp:?}");
        assert!(
            rendered.contains("api_impact artifact load"),
            "got: {rendered}"
        );
        assert!(rendered.contains("saturated"), "got: {rendered}");
    }

    #[test]
    fn timed_out_maps_to_mcp_internal_error_with_label() {
        let err = BlockingError::TimedOut {
            label: "bm25 index build",
            secs: 90,
        };
        let mcp: McpError = err.into();
        // Debug carries the message regardless of rmcp's exact field accessor.
        let rendered = format!("{mcp:?}");
        assert!(rendered.contains("bm25 index build"), "got: {rendered}");
        assert!(rendered.contains("90s"), "got: {rendered}");
    }
}
