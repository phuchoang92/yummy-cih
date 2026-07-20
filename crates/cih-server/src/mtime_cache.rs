//! Process-wide, single-flight cache keyed by a directory path. The cached
//! value carries its own freshness token (an mtime, a version, ...); this cache
//! adds **coalescing** (a burst of concurrent misses on the *same* key runs the
//! expensive loader once instead of N times) and **bounded retention** (an
//! entry cap with LRU eviction plus an idle TTL, so a long-lived multi-repo
//! server can't grow memory monotonically — S4 of the design record). Distinct
//! keys still load concurrently, and the value-read fast path never blocks on
//! a load.
//!
//! Shared by [`crate::xflow::XflowState`] and
//! [`crate::artifact_cache::ArtifactCache`], which previously each hand-rolled
//! the same check-then-load logic — minus the coalescing, so a cold or
//! just-reindexed key could stampede.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// Retention policy for an [`MtimeCache`]. `Default` is unlimited (used by
/// tests and any cache that manages its own lifecycle).
#[derive(Clone, Copy)]
pub(crate) struct CacheLimits {
    /// Max retained entries; least-recently-used beyond this are evicted.
    pub(crate) max_entries: usize,
    /// Entries untouched this long are evicted on the next load.
    pub(crate) idle_ttl: Duration,
}

impl Default for CacheLimits {
    fn default() -> Self {
        Self {
            max_entries: usize::MAX,
            idle_ttl: Duration::MAX,
        }
    }
}

impl CacheLimits {
    /// Artifact-cache policy from env: `CIH_ARTIFACT_CACHE_MAX_ENTRIES`
    /// (unset/invalid/0 = 32 — the §12.4 suggested repo cap) and
    /// `CIH_ARTIFACT_CACHE_IDLE_TTL_SECS` (unset/invalid = 1800; 0 disables
    /// the TTL). Shared by the two artifact-family caches; a byte-weighted
    /// budget is deliberately deferred to the Milestone 3 re-pricing.
    pub(crate) fn artifact_from_env() -> Self {
        let max_entries = std::env::var("CIH_ARTIFACT_CACHE_MAX_ENTRIES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(32);
        let ttl_secs = std::env::var("CIH_ARTIFACT_CACHE_IDLE_TTL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1800);
        Self {
            max_entries,
            idle_ttl: if ttl_secs == 0 {
                Duration::MAX
            } else {
                Duration::from_secs(ttl_secs)
            },
        }
    }
}

/// A cached value plus its recency bookkeeping. Atomics so a cache *hit* can
/// bump recency under the read lock.
struct Slot<V> {
    value: Arc<V>,
    /// Strict LRU order: global access-sequence number at last touch.
    touched_seq: AtomicU64,
    /// Wall-clock milliseconds at last touch, for the idle TTL.
    touched_at_ms: AtomicU64,
}

impl<V> Slot<V> {
    fn touch(&self) {
        self.touched_seq.store(next_seq(), Ordering::Relaxed);
        self.touched_at_ms.store(now_ms(), Ordering::Relaxed);
    }
}

/// Process-global access sequence — strictly increasing, so LRU ordering never
/// ties the way wall-clock timestamps can.
fn next_seq() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Single-flight, path-keyed value cache. `V` is expected to carry whatever
/// freshness token the caller checks in `is_fresh` (e.g. a `nodes.jsonl` mtime).
pub(crate) struct MtimeCache<V> {
    /// Fast path: the current value per key.
    cache: RwLock<HashMap<PathBuf, Slot<V>>>,
    /// Single-flight: a per-key gate that serializes loads of the *same* key.
    /// Evicted together with its entry; never held across anything but that
    /// key's `load()`.
    gates: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    limits: CacheLimits,
}

impl<V> Default for MtimeCache<V> {
    fn default() -> Self {
        Self::with_limits(CacheLimits::default())
    }
}

impl<V> MtimeCache<V> {
    pub(crate) fn with_limits(limits: CacheLimits) -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
            gates: Mutex::new(HashMap::new()),
            limits: CacheLimits {
                // A zero cap would evict the entry just inserted.
                max_entries: limits.max_entries.max(1),
                idle_ttl: limits.idle_ttl,
            },
        }
    }

    /// Return the cached value for `key` when `is_fresh` accepts it; otherwise
    /// load exactly once across concurrent callers and cache the result.
    /// `is_fresh` is evaluated under the read lock; `load` runs with no cache
    /// lock held, serialized per key so concurrent misses coalesce. Each load
    /// also applies the retention policy (idle TTL, then LRU down to the cap).
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
        let slot = Slot {
            value: value.clone(),
            touched_seq: AtomicU64::new(0),
            touched_at_ms: AtomicU64::new(0),
        };
        slot.touch();
        let evicted = {
            let mut cache = self.cache.write().unwrap_or_else(|e| e.into_inner());
            cache.insert(key, slot);
            self.evict_locked(&mut cache)
        };
        if !evicted.is_empty() {
            let mut gates = self.gates.lock().unwrap_or_else(|e| e.into_inner());
            for key in &evicted {
                gates.remove(key);
            }
        }
        Ok(value)
    }

    /// Apply the retention policy under the write lock: drop idle-TTL-expired
    /// entries, then the least-recently-used until at most `max_entries`
    /// remain. Returns the evicted keys so their gates can be dropped too.
    /// The just-inserted entry has the newest recency, so it always survives.
    fn evict_locked(&self, cache: &mut HashMap<PathBuf, Slot<V>>) -> Vec<PathBuf> {
        let mut evicted = Vec::new();
        if self.limits.idle_ttl != Duration::MAX {
            let cutoff_ms = now_ms().saturating_sub(self.limits.idle_ttl.as_millis() as u64);
            evicted.extend(
                cache
                    .iter()
                    .filter(|(_, slot)| slot.touched_at_ms.load(Ordering::Relaxed) < cutoff_ms)
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>(),
            );
            for key in &evicted {
                cache.remove(key);
            }
        }
        while cache.len() > self.limits.max_entries {
            let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, slot)| slot.touched_seq.load(Ordering::Relaxed))
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            cache.remove(&oldest);
            evicted.push(oldest);
        }
        evicted
    }

    fn peek(&self, key: &Path, is_fresh: &impl Fn(&V) -> bool) -> Option<Arc<V>> {
        let guard = self.cache.read().unwrap_or_else(|e| e.into_inner());
        let slot = guard.get(key)?;
        is_fresh(&slot.value).then(|| {
            slot.touch();
            slot.value.clone()
        })
    }

    fn gate_for(&self, key: &Path) -> Arc<Mutex<()>> {
        self.gates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.to_path_buf())
            .or_default()
            .clone()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.cache.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    #[cfg(test)]
    fn gate_count(&self) -> usize {
        self.gates.lock().unwrap_or_else(|e| e.into_inner()).len()
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

    /// S4: the entry cap evicts strictly least-recently-used (a hit refreshes
    /// recency), and the evicted key's gate goes with it.
    #[test]
    fn entry_cap_evicts_least_recently_used_and_its_gate() {
        let cache: MtimeCache<u32> = MtimeCache::with_limits(CacheLimits {
            max_entries: 2,
            idle_ttl: Duration::MAX,
        });
        let load = |cache: &MtimeCache<u32>, key: &str| {
            cache.get_or_load(key, |_| true, || Ok(0u32)).unwrap();
        };
        load(&cache, "a");
        load(&cache, "b");
        // Touch "a" so "b" is now the least recently used.
        load(&cache, "a");
        load(&cache, "c");
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.gate_count(), 2, "evicted gate must be dropped too");
        let survives = |key: &str| {
            let mut loaded = false;
            cache
                .get_or_load(
                    key,
                    |_| true,
                    || {
                        loaded = true;
                        Ok(0u32)
                    },
                )
                .unwrap();
            !loaded
        };
        assert!(survives("a"), "recently-touched entry must survive");
        assert!(survives("c"), "just-inserted entry must survive");
        assert!(!survives("b"), "least-recently-used entry must be evicted");
    }

    /// S4: entries idle past the TTL are dropped on the next load.
    #[test]
    fn idle_ttl_evicts_untouched_entries() {
        let cache: MtimeCache<u32> = MtimeCache::with_limits(CacheLimits {
            max_entries: usize::MAX,
            idle_ttl: Duration::from_millis(50),
        });
        cache.get_or_load("old", |_| true, || Ok(0u32)).unwrap();
        thread::sleep(Duration::from_millis(120));
        cache.get_or_load("new", |_| true, || Ok(0u32)).unwrap();
        assert_eq!(cache.len(), 1, "idle entry must be evicted");
        assert_eq!(cache.gate_count(), 1);
    }
}
