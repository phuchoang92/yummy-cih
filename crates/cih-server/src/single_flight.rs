//! Async, key-addressed single-flight cache: a burst of concurrent misses on
//! the same key runs the (async, fallible) initializer once; distinct keys
//! initialize concurrently; failures are never cached, so the next caller
//! retries. The async sibling of [`crate::mtime_cache::MtimeCache`] — that one
//! coalesces synchronous loaders, this one serves initializers that must
//! `.await` (graph-store connects, `ensure_schema`).

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

pub(crate) struct SingleFlight<V: Clone> {
    /// Fast path: the current value per key.
    values: RwLock<HashMap<String, V>>,
    /// Per-key gates serializing initialization of the *same* key. Grows one
    /// entry per distinct key — bounded by repo count, like `values`.
    gates: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl<V: Clone> SingleFlight<V> {
    /// Construct pre-seeded (e.g. the primary repo's already-connected store),
    /// so the default path never re-initializes.
    pub(crate) fn with(entries: impl IntoIterator<Item = (String, V)>) -> Self {
        Self {
            values: RwLock::new(entries.into_iter().collect()),
            gates: Mutex::new(HashMap::new()),
        }
    }

    /// Return the cached value for `key`, or run `init` exactly once across
    /// concurrent callers and cache its success. An `Err` is returned to the
    /// caller that ran it and nothing is cached — the next caller retries.
    pub(crate) async fn get_or_try_init<F, Fut, E>(&self, key: &str, init: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>>,
    {
        if let Some(v) = self.values.read().await.get(key) {
            return Ok(v.clone());
        }
        // Coalesce: hold this key's gate, then re-check — a racing caller may
        // have initialized (and cached) while we waited for the gate. The
        // gates map lock is never held across an `.await` of `init`.
        let gate = self
            .gates
            .lock()
            .await
            .entry(key.to_string())
            .or_default()
            .clone();
        let _held = gate.lock().await;
        if let Some(v) = self.values.read().await.get(key) {
            return Ok(v.clone());
        }
        let value = init().await?;
        self.values
            .write()
            .await
            .insert(key.to_string(), value.clone());
        Ok(value)
    }

    /// Infallible variant of [`get_or_try_init`](Self::get_or_try_init).
    pub(crate) async fn get_or_init<F, Fut>(&self, key: &str, init: F) -> V
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V>,
    {
        match self
            .get_or_try_init::<_, _, std::convert::Infallible>(key, || async { Ok(init().await) })
            .await
        {
            Ok(v) => v,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// The canonical single-flight proof: 16 concurrent misses on one key run
    /// the initializer once, and every caller observes the same value.
    #[tokio::test(flavor = "multi_thread")]
    async fn coalesces_concurrent_misses_to_a_single_init() {
        let cache: Arc<SingleFlight<usize>> = Arc::new(SingleFlight::with([]));
        let inits = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let cache = cache.clone();
            let inits = inits.clone();
            handles.push(tokio::spawn(async move {
                cache
                    .get_or_try_init::<_, _, String>("graph-key", || async {
                        // Sleep so every task is inside the miss window before
                        // the first init completes.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        inits.fetch_add(1, Ordering::SeqCst);
                        Ok(7usize)
                    })
                    .await
                    .unwrap()
            }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap(), 7);
        }
        assert_eq!(
            inits.load(Ordering::SeqCst),
            1,
            "initializer must run once despite 16 concurrent misses"
        );
    }

    /// §14.2: failures are not cached — a later caller retries and succeeds.
    #[tokio::test]
    async fn failure_is_not_cached_and_the_next_caller_retries() {
        let cache: SingleFlight<usize> = SingleFlight::with([]);
        let attempts = AtomicUsize::new(0);
        let run = |succeed: bool| {
            let attempts = &attempts;
            cache.get_or_try_init("k", move || async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                if succeed {
                    Ok(42usize)
                } else {
                    Err("backend down".to_string())
                }
            })
        };
        assert_eq!(run(false).await.unwrap_err(), "backend down");
        assert_eq!(run(true).await.unwrap(), 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        // Now cached: no third attempt.
        assert_eq!(run(false).await.unwrap(), 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn distinct_keys_initialize_independently_and_seeds_are_served() {
        let cache: SingleFlight<&'static str> =
            SingleFlight::with([("seeded".to_string(), "primary")]);
        assert_eq!(
            cache.get_or_init("seeded", || async { "never-runs" }).await,
            "primary"
        );
        assert_eq!(
            cache.get_or_init("other", || async { "built" }).await,
            "built"
        );
        assert_eq!(
            cache.get_or_init("other", || async { "stale" }).await,
            "built"
        );
    }
}
