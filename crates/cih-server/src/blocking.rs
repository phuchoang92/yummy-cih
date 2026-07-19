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
use std::sync::OnceLock;
use std::time::Duration;

use rmcp::ErrorData as McpError;

/// Failure of a deadline-guarded blocking task.
#[derive(Debug)]
pub(crate) enum BlockingError {
    /// The task exceeded its deadline (and, per the module note, is still running).
    TimedOut { label: &'static str, secs: u64 },
    /// The task panicked (surfaced as a `JoinError`).
    Panicked { label: &'static str, detail: String },
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
