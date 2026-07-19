//! Process-wide, single-flight cache keyed by a directory path. The cached
//! value carries its own freshness token (an mtime, a version, ...); this cache
//! adds only **coalescing**, so a burst of concurrent misses on the *same* key
//! runs the (expensive) loader once instead of N times. Distinct keys still load
//! concurrently, and the value-read fast path never blocks on a load.
//!
//! Shared by [`crate::xflow::XflowState`] and
//! [`crate::artifact_cache::ArtifactCache`], which previously each hand-rolled
//! the same check-then-load logic — minus the coalescing, so a cold or
//! just-reindexed key could stampede.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

/// Single-flight, path-keyed value cache. `V` is expected to carry whatever
/// freshness token the caller checks in `is_fresh` (e.g. a `nodes.jsonl` mtime).
pub(crate) struct MtimeCache<V> {
    /// Fast path: the current value per key.
    cache: RwLock<HashMap<PathBuf, Arc<V>>>,
    /// Single-flight: a per-key gate that serializes loads of the *same* key.
    /// Grows one entry per distinct key (bounded by repo count, like `cache`);
    /// never held across anything but that key's `load()`.
    gates: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl<V> Default for MtimeCache<V> {
    fn default() -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
            gates: Mutex::new(HashMap::new()),
        }
    }
}

impl<V> MtimeCache<V> {
    /// Return the cached value for `key` when `is_fresh` accepts it; otherwise
    /// load exactly once across concurrent callers and cache the result.
    /// `is_fresh` is evaluated under the read lock; `load` runs with no cache
    /// lock held, serialized per key so concurrent misses coalesce.
    pub(crate) fn get_or_load(
        &self,
        key: &str,
        is_fresh: impl Fn(&V) -> bool,
        load: impl FnOnce() -> std::io::Result<V>,
    ) -> std::io::Result<Arc<V>> {
        let key = PathBuf::from(key);
        if let Some(hit) = self.peek(&key, &is_fresh) {
            return Ok(hit);
        }
        // Coalesce: hold this key's gate, then re-check — a racing caller may
        // have loaded (and cached) while we waited for the gate.
        let gate = self.gate_for(&key);
        let _held = gate.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = self.peek(&key, &is_fresh) {
            return Ok(hit);
        }
        let value = Arc::new(load()?);
        self.cache
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, value.clone());
        Ok(value)
    }

    fn peek(&self, key: &Path, is_fresh: &impl Fn(&V) -> bool) -> Option<Arc<V>> {
        let guard = self.cache.read().unwrap_or_else(|e| e.into_inner());
        let value = guard.get(key)?;
        is_fresh(value).then(|| value.clone())
    }

    fn gate_for(&self, key: &Path) -> Arc<Mutex<()>> {
        self.gates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.to_path_buf())
            .or_default()
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    /// The canonical single-flight proof: 16 concurrent misses on one key run
    /// the loader **once** (without coalescing this would be 16), and every
    /// caller observes the same shared `Arc`.
    #[test]
    fn coalesces_concurrent_misses_to_a_single_load() {
        let cache: Arc<MtimeCache<usize>> = Arc::new(MtimeCache::default());
        let loads = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..16)
            .map(|_| {
                let cache = cache.clone();
                let loads = loads.clone();
                thread::spawn(move || {
                    cache
                        .get_or_load(
                            "artifacts/dir",
                            |_| true, // once a value is present it's fresh
                            || {
                                // Sleep so every thread is inside the miss window
                                // before the first load completes.
                                thread::sleep(Duration::from_millis(50));
                                loads.fetch_add(1, Ordering::SeqCst);
                                Ok(7usize)
                            },
                        )
                        .unwrap()
                })
            })
            .collect();
        let results: Vec<Arc<usize>> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        assert_eq!(
            loads.load(Ordering::SeqCst),
            1,
            "loader must run once despite 16 concurrent misses"
        );
        let first = results[0].clone();
        assert!(
            results.iter().all(|r| Arc::ptr_eq(r, &first)),
            "all callers must share one value"
        );
        assert_eq!(*first, 7);
    }

    /// A stale value (freshness predicate rejects it) reloads; a fresh one hits.
    #[test]
    fn reloads_when_stale_hits_when_fresh() {
        let cache: MtimeCache<u32> = MtimeCache::default();
        let loads = AtomicUsize::new(0);
        let run = |fresh: bool| {
            cache
                .get_or_load(
                    "k",
                    |_| fresh,
                    || {
                        loads.fetch_add(1, Ordering::SeqCst);
                        Ok(1u32)
                    },
                )
                .unwrap()
        };
        run(false); // cold miss → load #1
        run(false); // present but stale → load #2
        assert_eq!(loads.load(Ordering::SeqCst), 2);
        run(true); // present and fresh → cache hit, no load
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }
}
