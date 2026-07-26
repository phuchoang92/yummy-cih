use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::model::EmbedModel;

/// One model call at a time is the conservative serving default. Fastembed's
/// model is mutex-protected, so admitting more work only grows the blocking
/// pool backlog unless a future model implementation supports true parallel
/// inference.
pub const DEFAULT_EMBED_INFERENCE_MAX_CONCURRENT: usize = 1;
/// Requests may wait briefly for the inference lane before being shed.
pub const DEFAULT_EMBED_INFERENCE_QUEUE_TIMEOUT_MS: u64 = 250;
/// Leaves headroom in the documented 1500 ms total semantic-search budget for
/// queue admission, the Postgres statement, and result merge.
pub const DEFAULT_EMBED_INFERENCE_TIMEOUT_MS: u64 = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmbedInferenceConfig {
    max_concurrent: usize,
    queue_timeout: Duration,
    inference_timeout: Duration,
}

impl EmbedInferenceConfig {
    pub fn new(
        max_concurrent: usize,
        queue_timeout: Duration,
        inference_timeout: Duration,
    ) -> Result<Self> {
        if max_concurrent == 0 {
            return Err(anyhow!(
                "CIH_EMBED_INFERENCE_MAX_CONCURRENT must be greater than zero"
            ));
        }
        if queue_timeout.is_zero() {
            return Err(anyhow!(
                "CIH_EMBED_INFERENCE_QUEUE_TIMEOUT_MS must be greater than zero"
            ));
        }
        if inference_timeout.is_zero() {
            return Err(anyhow!(
                "CIH_EMBED_INFERENCE_TIMEOUT_MS must be greater than zero"
            ));
        }
        Ok(Self {
            max_concurrent,
            queue_timeout,
            inference_timeout,
        })
    }

    /// Compatibility parser for engine/CLI callers of `EmbedStore::connect`.
    /// The server validates the same knobs once in its `RetrievalConfig` and
    /// injects this typed value through `connect_with_inference_config`.
    pub fn from_env() -> Result<Self> {
        Self::new(
            positive_env(
                "CIH_EMBED_INFERENCE_MAX_CONCURRENT",
                DEFAULT_EMBED_INFERENCE_MAX_CONCURRENT,
            )?,
            Duration::from_millis(positive_env(
                "CIH_EMBED_INFERENCE_QUEUE_TIMEOUT_MS",
                DEFAULT_EMBED_INFERENCE_QUEUE_TIMEOUT_MS,
            )?),
            Duration::from_millis(positive_env(
                "CIH_EMBED_INFERENCE_TIMEOUT_MS",
                DEFAULT_EMBED_INFERENCE_TIMEOUT_MS,
            )?),
        )
    }

    pub fn max_concurrent(self) -> usize {
        self.max_concurrent
    }

    pub fn queue_timeout(self) -> Duration {
        self.queue_timeout
    }

    pub fn inference_timeout(self) -> Duration {
        self.inference_timeout
    }
}

impl Default for EmbedInferenceConfig {
    fn default() -> Self {
        Self {
            max_concurrent: DEFAULT_EMBED_INFERENCE_MAX_CONCURRENT,
            queue_timeout: Duration::from_millis(DEFAULT_EMBED_INFERENCE_QUEUE_TIMEOUT_MS),
            inference_timeout: Duration::from_millis(DEFAULT_EMBED_INFERENCE_TIMEOUT_MS),
        }
    }
}

fn positive_env<T>(name: &'static str, default: T) -> Result<T>
where
    T: std::str::FromStr + PartialEq + Default + Copy,
{
    let value = match std::env::var(name) {
        Ok(raw) => raw
            .parse::<T>()
            .map_err(|_| anyhow!("{name} must be a positive integer (got '{raw}')"))?,
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => return Err(anyhow!("cannot read {name}: {error}")),
    };
    if value == T::default() {
        return Err(anyhow!("{name} must be greater than zero"));
    }
    Ok(value)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EmbedInferenceMetricsSnapshot {
    pub active: usize,
    pub peak_active: usize,
    pub queued: usize,
    pub rejected: u64,
    pub timed_out: u64,
    pub panicked: u64,
    pub completed: u64,
    pub queue_wait_ms: u64,
    pub elapsed_ms: u64,
}

#[derive(Default)]
struct EmbedInferenceMetrics {
    active: AtomicUsize,
    peak_active: AtomicUsize,
    queued: AtomicUsize,
    rejected: AtomicU64,
    timed_out: AtomicU64,
    panicked: AtomicU64,
    completed: AtomicU64,
    queue_wait_ms: AtomicU64,
    elapsed_ms: AtomicU64,
}

impl EmbedInferenceMetrics {
    fn snapshot(&self) -> EmbedInferenceMetricsSnapshot {
        EmbedInferenceMetricsSnapshot {
            active: self.active.load(Ordering::Relaxed),
            peak_active: self.peak_active.load(Ordering::Relaxed),
            queued: self.queued.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            timed_out: self.timed_out.load(Ordering::Relaxed),
            panicked: self.panicked.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            queue_wait_ms: self.queue_wait_ms.load(Ordering::Relaxed),
            elapsed_ms: self.elapsed_ms.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
pub enum EmbedInferenceError {
    Saturated {
        waited_ms: u128,
        max_concurrent: usize,
    },
    TimedOut {
        timeout_ms: u128,
    },
    Closed(String),
    Panicked(String),
    Model(anyhow::Error),
}

impl EmbedInferenceError {
    pub fn is_saturated(&self) -> bool {
        matches!(self, Self::Saturated { .. })
    }

    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::TimedOut { .. })
    }
}

impl fmt::Display for EmbedInferenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Saturated {
                waited_ms,
                max_concurrent,
            } => write!(
                f,
                "embedding inference capacity saturated after {waited_ms}ms \
                 (max_concurrent={max_concurrent}); tune \
                 CIH_EMBED_INFERENCE_MAX_CONCURRENT and \
                 CIH_EMBED_INFERENCE_QUEUE_TIMEOUT_MS"
            ),
            Self::TimedOut { timeout_ms } => write!(
                f,
                "embedding inference timed out after {timeout_ms}ms; tune \
                 CIH_EMBED_INFERENCE_TIMEOUT_MS only after measuring model latency"
            ),
            Self::Closed(detail) => write!(f, "embedding inference lane closed: {detail}"),
            Self::Panicked(detail) => write!(f, "embedding inference task panicked: {detail}"),
            Self::Model(error) => write!(f, "embedding inference failed: {error}"),
        }
    }
}

impl std::error::Error for EmbedInferenceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Model(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct EmbedInferenceRuntime {
    lane: Arc<Semaphore>,
    config: EmbedInferenceConfig,
    metrics: Arc<EmbedInferenceMetrics>,
}

impl EmbedInferenceRuntime {
    pub(crate) fn new(config: EmbedInferenceConfig) -> Self {
        Self {
            lane: Arc::new(Semaphore::new(config.max_concurrent)),
            config,
            metrics: Arc::new(EmbedInferenceMetrics::default()),
        }
    }

    pub(crate) fn metrics(&self) -> EmbedInferenceMetricsSnapshot {
        self.metrics.snapshot()
    }

    pub(crate) async fn embed_query(
        &self,
        model: Arc<EmbedModel>,
        texts: Vec<String>,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedInferenceError> {
        self.run_request(move || model.embed(&texts)).await
    }

    pub(crate) async fn embed_batch(
        &self,
        model: Arc<EmbedModel>,
        texts: Vec<String>,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedInferenceError> {
        self.run(None, move || model.embed(&texts)).await
    }

    async fn run_request<T, F>(&self, work: F) -> std::result::Result<T, EmbedInferenceError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        self.run(Some(self.config.inference_timeout), work).await
    }

    async fn run<T, F>(
        &self,
        deadline: Option<Duration>,
        work: F,
    ) -> std::result::Result<T, EmbedInferenceError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        let queued = QueuedGuard::enter(self.metrics.clone());
        let permit = match tokio::time::timeout(
            self.config.queue_timeout,
            self.lane.clone().acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(error)) => return Err(EmbedInferenceError::Closed(error.to_string())),
            Err(_) => {
                self.metrics.rejected.fetch_add(1, Ordering::Relaxed);
                return Err(EmbedInferenceError::Saturated {
                    waited_ms: self.config.queue_timeout.as_millis(),
                    max_concurrent: self.config.max_concurrent,
                });
            }
        };
        drop(queued);

        // Account for the admitted job before handing it to Tokio, then move
        // the guard (and its permit) into the uncancellable closure. This
        // keeps both the gauge and capacity truthful even if the awaiting
        // request is cancelled before the blocking task begins executing.
        let active = ActiveInferenceGuard::enter(self.metrics.clone(), permit);
        let metrics = self.metrics.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _active = active;
            let started = Instant::now();
            let result = work().map_err(EmbedInferenceError::Model);
            metrics.completed.fetch_add(1, Ordering::Relaxed);
            metrics
                .elapsed_ms
                .fetch_add(elapsed_ms(started), Ordering::Relaxed);
            result
        });

        let Some(deadline) = deadline else {
            return task.await.map_err(|error| {
                self.metrics.panicked.fetch_add(1, Ordering::Relaxed);
                EmbedInferenceError::Panicked(error.to_string())
            })?;
        };

        match tokio::time::timeout(deadline, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                self.metrics.panicked.fetch_add(1, Ordering::Relaxed);
                Err(EmbedInferenceError::Panicked(error.to_string()))
            }
            Err(_) => {
                // `spawn_blocking` cannot be cancelled once running. Its
                // closure still owns both the lane permit and active gauge, so
                // a caller timeout/cancellation cannot admit replacement work
                // before the actual inference finishes.
                self.metrics.timed_out.fetch_add(1, Ordering::Relaxed);
                Err(EmbedInferenceError::TimedOut {
                    timeout_ms: deadline.as_millis(),
                })
            }
        }
    }
}

struct QueuedGuard {
    metrics: Arc<EmbedInferenceMetrics>,
    started: Instant,
}

impl QueuedGuard {
    fn enter(metrics: Arc<EmbedInferenceMetrics>) -> Self {
        metrics.queued.fetch_add(1, Ordering::Relaxed);
        Self {
            metrics,
            started: Instant::now(),
        }
    }
}

impl Drop for QueuedGuard {
    fn drop(&mut self) {
        self.metrics.queued.fetch_sub(1, Ordering::Relaxed);
        self.metrics
            .queue_wait_ms
            .fetch_add(elapsed_ms(self.started), Ordering::Relaxed);
    }
}

struct ActiveInferenceGuard {
    metrics: Arc<EmbedInferenceMetrics>,
    _permit: OwnedSemaphorePermit,
}

impl ActiveInferenceGuard {
    fn enter(metrics: Arc<EmbedInferenceMetrics>, permit: OwnedSemaphorePermit) -> Self {
        let active = metrics
            .active
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        metrics.peak_active.fetch_max(active, Ordering::Relaxed);
        Self {
            metrics,
            _permit: permit,
        }
    }
}

impl Drop for ActiveInferenceGuard {
    fn drop(&mut self) {
        self.metrics.active.fetch_sub(1, Ordering::Relaxed);
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(queue_ms: u64, inference_ms: u64) -> EmbedInferenceRuntime {
        EmbedInferenceRuntime::new(
            EmbedInferenceConfig::new(
                1,
                Duration::from_millis(queue_ms),
                Duration::from_millis(inference_ms),
            )
            .unwrap(),
        )
    }

    async fn wait_for_active(runtime: &EmbedInferenceRuntime, expected: usize) {
        tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                if runtime.metrics().active == expected {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("inference activity did not reach the expected value");
    }

    #[test]
    fn inference_config_rejects_zero_values_with_named_knobs() {
        let error =
            EmbedInferenceConfig::new(0, Duration::from_millis(1), Duration::from_millis(1))
                .unwrap_err();
        assert!(error
            .to_string()
            .contains("CIH_EMBED_INFERENCE_MAX_CONCURRENT"));

        let error =
            EmbedInferenceConfig::new(1, Duration::ZERO, Duration::from_millis(1)).unwrap_err();
        assert!(error
            .to_string()
            .contains("CIH_EMBED_INFERENCE_QUEUE_TIMEOUT_MS"));

        let error =
            EmbedInferenceConfig::new(1, Duration::from_millis(1), Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("CIH_EMBED_INFERENCE_TIMEOUT_MS"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inference_does_not_block_the_async_worker() {
        let runtime = runtime(100, 500);
        let inference = runtime.run_request(|| {
            std::thread::sleep(Duration::from_millis(100));
            Ok::<_, anyhow::Error>(42)
        });
        tokio::pin!(inference);

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            result = &mut inference => panic!("blocking work returned unexpectedly: {result:?}"),
        }

        assert_eq!(inference.await.unwrap(), 42);
        assert_eq!(runtime.metrics().peak_active, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timeout_keeps_capacity_until_uncancellable_inference_finishes() {
        let runtime = runtime(25, 30);
        let first = runtime
            .run_request(|| {
                std::thread::sleep(Duration::from_millis(180));
                Ok::<_, anyhow::Error>(1)
            })
            .await;
        assert!(matches!(first, Err(EmbedInferenceError::TimedOut { .. })));
        assert_eq!(runtime.metrics().active, 1);

        let second = runtime.run_request(|| Ok::<_, anyhow::Error>(2)).await;
        let error = second.unwrap_err();
        assert!(error.is_saturated());
        let message = error.to_string();
        assert!(message.contains("CIH_EMBED_INFERENCE_MAX_CONCURRENT"));
        assert!(message.contains("CIH_EMBED_INFERENCE_QUEUE_TIMEOUT_MS"));

        wait_for_active(&runtime, 0).await;
        assert_eq!(
            runtime
                .run_request(|| Ok::<_, anyhow::Error>(3))
                .await
                .unwrap(),
            3
        );
        let metrics = runtime.metrics();
        assert_eq!(metrics.peak_active, 1);
        assert_eq!(metrics.timed_out, 1);
        assert_eq!(metrics.rejected, 1);
        assert_eq!(metrics.completed, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_waiter_does_not_release_running_inference_capacity() {
        let runtime = runtime(25, 1_000);
        let task_runtime = runtime.clone();
        let task = tokio::spawn(async move {
            task_runtime
                .run_request(|| {
                    std::thread::sleep(Duration::from_millis(180));
                    Ok::<_, anyhow::Error>(())
                })
                .await
        });
        wait_for_active(&runtime, 1).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        let replacement = runtime.run_request(|| Ok::<_, anyhow::Error>(())).await;
        assert!(matches!(
            replacement,
            Err(EmbedInferenceError::Saturated { .. })
        ));
        assert_eq!(runtime.metrics().active, 1);

        wait_for_active(&runtime, 0).await;
        assert_eq!(runtime.metrics().peak_active, 1);
    }
}
