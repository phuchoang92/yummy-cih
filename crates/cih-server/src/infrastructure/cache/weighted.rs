//! Small async weighted-LRU cache used by wiki and search state.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

struct Slot<V> {
    value: Arc<V>,
    weight_bytes: usize,
    touched: AtomicU64,
}

impl<V> Slot<V> {
    fn touch(&self) {
        self.touched.store(next_seq(), Ordering::Relaxed);
    }
}

fn next_seq() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AsyncCacheMetrics {
    pub(crate) requests: u64,
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) builds: u64,
    pub(crate) retained_entries: usize,
    pub(crate) retained_weight_bytes: usize,
    pub(crate) evictions: u64,
    pub(crate) oversize: u64,
}

#[derive(Default)]
struct Counters {
    requests: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    builds: AtomicU64,
    evictions: AtomicU64,
    oversize: AtomicU64,
}

pub(crate) struct InsertResult<K> {
    pub(crate) retained: bool,
    pub(crate) removed_keys: Vec<K>,
}

pub(crate) struct AsyncWeightedCache<K, V> {
    values: tokio::sync::RwLock<HashMap<K, Slot<V>>>,
    max_entries: usize,
    max_weight_bytes: usize,
    counters: Counters,
}

impl<K, V> AsyncWeightedCache<K, V>
where
    K: Clone + Eq + Hash,
{
    pub(crate) fn new(max_entries: usize, max_weight_bytes: usize) -> Self {
        Self {
            values: tokio::sync::RwLock::new(HashMap::new()),
            max_entries: max_entries.max(1),
            max_weight_bytes: max_weight_bytes.max(1),
            counters: Counters::default(),
        }
    }

    pub(crate) async fn get_if(
        &self,
        key: &K,
        predicate: impl FnOnce(&V) -> bool,
    ) -> Option<Arc<V>> {
        self.counters.requests.fetch_add(1, Ordering::Relaxed);
        let values = self.values.read().await;
        let hit = values
            .get(key)
            .filter(|slot| predicate(&slot.value))
            .map(|slot| {
                slot.touch();
                slot.value.clone()
            });
        if hit.is_some() {
            self.counters.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters.misses.fetch_add(1, Ordering::Relaxed);
        }
        hit
    }

    pub(crate) async fn insert(
        &self,
        key: K,
        value: Arc<V>,
        weight_bytes: usize,
    ) -> InsertResult<K> {
        let mut values = self.values.write().await;
        self.counters.builds.fetch_add(1, Ordering::Relaxed);
        let mut removed_keys = Vec::new();
        if weight_bytes > self.max_weight_bytes {
            if values.remove(&key).is_some() {
                removed_keys.push(key);
            }
            self.counters.oversize.fetch_add(1, Ordering::Relaxed);
            return InsertResult {
                retained: false,
                removed_keys,
            };
        }

        let slot = Slot {
            value,
            weight_bytes,
            touched: AtomicU64::new(0),
        };
        slot.touch();
        values.insert(key, slot);
        while values.len() > self.max_entries || retained_weight(&values) > self.max_weight_bytes {
            let Some(oldest) = values
                .iter()
                .min_by_key(|(_, slot)| slot.touched.load(Ordering::Relaxed))
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            values.remove(&oldest);
            removed_keys.push(oldest);
            self.counters.evictions.fetch_add(1, Ordering::Relaxed);
        }
        InsertResult {
            retained: true,
            removed_keys,
        }
    }

    pub(crate) async fn metrics(&self) -> AsyncCacheMetrics {
        let values = self.values.read().await;
        AsyncCacheMetrics {
            requests: self.counters.requests.load(Ordering::Relaxed),
            hits: self.counters.hits.load(Ordering::Relaxed),
            misses: self.counters.misses.load(Ordering::Relaxed),
            builds: self.counters.builds.load(Ordering::Relaxed),
            retained_entries: values.len(),
            retained_weight_bytes: retained_weight(&values),
            evictions: self.counters.evictions.load(Ordering::Relaxed),
            oversize: self.counters.oversize.load(Ordering::Relaxed),
        }
    }
}

fn retained_weight<K, V>(values: &HashMap<K, Slot<V>>) -> usize {
    values.values().fold(0usize, |total, slot| {
        total.saturating_add(slot.weight_bytes)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn weighted_lru_evicts_oldest_and_oversize_does_not_flush() {
        let cache = AsyncWeightedCache::new(10, 8);
        cache.insert("a", Arc::new(1), 4).await;
        cache.insert("b", Arc::new(2), 4).await;
        assert!(cache.get_if(&"a", |_| true).await.is_some());
        cache.insert("c", Arc::new(3), 4).await;
        assert!(cache.get_if(&"a", |_| true).await.is_some());
        assert!(cache.get_if(&"b", |_| true).await.is_none());
        assert!(cache.get_if(&"c", |_| true).await.is_some());

        let result = cache.insert("oversize", Arc::new(9), 9).await;
        assert!(!result.retained);
        assert!(cache.get_if(&"a", |_| true).await.is_some());
        assert!(cache.get_if(&"c", |_| true).await.is_some());
        let metrics = cache.metrics().await;
        assert_eq!(metrics.retained_weight_bytes, 8);
        assert_eq!(metrics.evictions, 1);
        assert_eq!(metrics.oversize, 1);
    }
}
