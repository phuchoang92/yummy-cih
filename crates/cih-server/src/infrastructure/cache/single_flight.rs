//! Async, key-addressed single-flight cache: a burst of concurrent misses on
//! the same key runs the (async, fallible) initializer once; distinct keys
//! initialize concurrently; failures are never cached, so the next caller
//! retries. The async sibling of the mtime cache — that one
//! coalesces synchronous loaders, this one serves initializers that must
//! `.await` (graph-store connects, `ensure_schema`).

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{Mutex, Notify, RwLock};

enum FlightState<V, E> {
    Running { participants: usize },
    Complete(Result<V, E>),
    Abandoned,
}

enum ObservedFlight<V, E> {
    Running,
    Complete(Result<V, E>),
    Abandoned,
}

struct Flight<V, E> {
    state: StdMutex<FlightState<V, E>>,
    completed: Notify,
}

impl<V, E> Flight<V, E> {
    fn running() -> Self {
        Self {
            state: StdMutex::new(FlightState::Running { participants: 1 }),
            completed: Notify::new(),
        }
    }
}

struct LeaderGuard<'a, V, E> {
    flight: &'a Flight<V, E>,
    published: bool,
}

impl<V, E> Drop for LeaderGuard<'_, V, E> {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        *self
            .flight
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = FlightState::Abandoned;
        self.flight.completed.notify_waiters();
    }
}

pub(crate) struct SingleFlight<V: Clone, E: Clone = Infallible> {
    /// Fast path: the current value per key.
    values: RwLock<HashMap<String, V>>,
    /// One active generation per key. Completed generations are removed before
    /// publishing their result, so failures reach current waiters but are not
    /// retained for later requests.
    flights: Mutex<HashMap<String, Arc<Flight<V, E>>>>,
}

impl<V: Clone, E: Clone> SingleFlight<V, E> {
    /// Construct pre-seeded (e.g. the primary repo's already-connected store),
    /// so the default path never re-initializes.
    pub(crate) fn with(entries: impl IntoIterator<Item = (String, V)>) -> Self {
        Self {
            values: RwLock::new(entries.into_iter().collect()),
            flights: Mutex::new(HashMap::new()),
        }
    }

    /// Return the cached value for `key`, or run `init` exactly once across
    /// concurrent callers and cache its success. All callers waiting on one
    /// generation receive the same cloned result. A failed generation is
    /// removed before publication, so a later caller starts a fresh attempt.
    pub(crate) async fn get_or_try_init<F, Fut>(&self, key: &str, init: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>>,
    {
        let mut init = Some(init);
        loop {
            if let Some(value) = self.values.read().await.get(key) {
                return Ok(value.clone());
            }

            let (flight, leader) = {
                let mut flights = self.flights.lock().await;
                let existing = flights.get(key).cloned().filter(|flight| {
                    let mut state = flight
                        .state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    match &mut *state {
                        FlightState::Running { participants } => {
                            *participants += 1;
                            true
                        }
                        FlightState::Complete(_) => true,
                        FlightState::Abandoned => false,
                    }
                });
                if let Some(existing) = existing {
                    (existing, false)
                } else {
                    flights.remove(key);
                    let flight = Arc::new(Flight::running());
                    flights.insert(key.to_string(), flight.clone());
                    (flight, true)
                }
            };

            if leader {
                let mut guard = LeaderGuard {
                    flight: &flight,
                    published: false,
                };
                let result = init.take().expect("initializer consumed once")().await;
                if let Ok(value) = &result {
                    self.values
                        .write()
                        .await
                        .insert(key.to_string(), value.clone());
                }

                // Remove the generation before publishing. No await occurs
                // after publication, so a failure cannot remain cached if this
                // caller is cancelled while returning.
                let mut flights = self.flights.lock().await;
                if flights
                    .get(key)
                    .is_some_and(|current| Arc::ptr_eq(current, &flight))
                {
                    flights.remove(key);
                }
                drop(flights);
                *flight
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) =
                    FlightState::Complete(result.clone());
                guard.published = true;
                flight.completed.notify_waiters();
                return result;
            }

            loop {
                // Register before checking state to avoid losing a completion
                // notification between the check and `.await`.
                let notified = flight.completed.notified();
                let observed = {
                    let state = flight
                        .state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    match &*state {
                        FlightState::Complete(result) => ObservedFlight::Complete(result.clone()),
                        FlightState::Abandoned => ObservedFlight::Abandoned,
                        FlightState::Running { .. } => ObservedFlight::Running,
                    }
                };
                match observed {
                    ObservedFlight::Complete(result) => return result,
                    ObservedFlight::Abandoned => break,
                    ObservedFlight::Running => notified.await,
                }
            }
        }
    }
}

impl<V: Clone> SingleFlight<V, Infallible> {
    /// Infallible variant of [`get_or_try_init`](Self::get_or_try_init).
    pub(crate) async fn get_or_init<F, Fut>(&self, key: &str, init: F) -> V
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V>,
    {
        match self
            .get_or_try_init(key, || async { Ok(init().await) })
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
        let cache: Arc<SingleFlight<usize, String>> = Arc::new(SingleFlight::with([]));
        let inits = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let cache = cache.clone();
            let inits = inits.clone();
            handles.push(tokio::spawn(async move {
                cache
                    .get_or_try_init("graph-key", || async {
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
        let cache: SingleFlight<usize, String> = SingleFlight::with([]);
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

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_waiters_share_one_failure_then_later_call_retries() {
        const CALLERS: usize = 32;
        let cache: Arc<SingleFlight<usize, String>> = Arc::new(SingleFlight::with([]));
        let attempts = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(tokio::sync::Barrier::new(CALLERS + 1));
        let release = Arc::new(Notify::new());
        let mut handles = Vec::new();

        for _ in 0..CALLERS {
            let cache = cache.clone();
            let attempts = attempts.clone();
            let start = start.clone();
            let release = release.clone();
            handles.push(tokio::spawn(async move {
                start.wait().await;
                cache
                    .get_or_try_init("graph-key", || async move {
                        let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                        release.notified().await;
                        Err(format!("planned failure {attempt}"))
                    })
                    .await
            }));
        }
        start.wait().await;

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let participants = {
                    let flights = cache.flights.lock().await;
                    flights.get("graph-key").map(|flight| {
                        let state = flight
                            .state
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        match &*state {
                            FlightState::Running { participants } => *participants,
                            _ => 0,
                        }
                    })
                };
                if participants == Some(CALLERS) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all callers should join the active generation");
        release.notify_waiters();

        for handle in handles {
            assert_eq!(handle.await.unwrap().unwrap_err(), "planned failure 1");
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 1);

        let value = cache
            .get_or_try_init("graph-key", || async {
                attempts.fetch_add(1, Ordering::SeqCst);
                Ok(42)
            })
            .await
            .unwrap();
        assert_eq!(value, 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_leader_allows_a_new_generation() {
        let cache: Arc<SingleFlight<usize, String>> = Arc::new(SingleFlight::with([]));
        let started = Arc::new(Notify::new());
        let leader = {
            let cache = cache.clone();
            let started = started.clone();
            tokio::spawn(async move {
                cache
                    .get_or_try_init("k", || async move {
                        started.notify_one();
                        std::future::pending::<Result<usize, String>>().await
                    })
                    .await
            })
        };
        started.notified().await;
        leader.abort();
        assert!(leader.await.unwrap_err().is_cancelled());

        let value = tokio::time::timeout(
            Duration::from_secs(1),
            cache.get_or_try_init("k", || async { Ok(7) }),
        )
        .await
        .expect("abandoned generation must not strand the next caller")
        .unwrap();
        assert_eq!(value, 7);
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
