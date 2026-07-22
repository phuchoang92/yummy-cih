//! Local hybrid-search index provider, bounded execution, and cache policy.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use cih_embed::{EmbedStore, SemanticHit};
use cih_search::{
    rrf_merge, search_index_path, search_schema_fingerprint, SearchHit, SearchIndex,
    SearchIndexInspection, SearchIndexLoad, SearchIndexSource,
};
use tokio::sync::{Notify, Semaphore};

use crate::infrastructure::cache::weighted::{AsyncCacheMetrics, AsyncWeightedCache};
use crate::infrastructure::file_search_index_store::FileSearchIndexStore;
use crate::ports::blocking_runtime::{
    blocking_timeout, run_blocking, run_blocking_heavy, BlockingError,
};
use crate::ports::retrieval_metrics::{SearchIndexMetricsSnapshot, SearchRuntimeMetricsSnapshot};
use crate::ports::search_index_store::SearchIndexStore;
use crate::ports::search_provider::{SearchProvider, SearchProviderError};

const MIB: usize = 1024 * 1024;
const WARNING_KEY_LIMIT: usize = 1024;

#[derive(Clone, Debug, Eq)]
struct SearchIndexKey {
    artifacts_root: PathBuf,
    artifact_version: String,
    schema: [u8; 32],
}

impl PartialEq for SearchIndexKey {
    fn eq(&self, other: &Self) -> bool {
        self.artifacts_root == other.artifacts_root
            && self.artifact_version == other.artifact_version
            && self.schema == other.schema
    }
}

impl Hash for SearchIndexKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.artifacts_root.hash(state);
        self.artifact_version.hash(state);
        self.schema.hash(state);
    }
}

#[derive(Default)]
struct SearchRuntimeMetrics {
    scorer_active: AtomicUsize,
    scorer_peak_active: AtomicUsize,
    scorer_queued: AtomicUsize,
    scorer_rejected: AtomicU64,
    scorer_queue_wait_ms: AtomicU64,
    scorer_scratch_bytes: AtomicUsize,
    scorer_peak_scratch_bytes: AtomicUsize,
    scorer_peak_per_query_scratch_bytes: AtomicUsize,
    score_completed: AtomicU64,
    score_elapsed_ms: AtomicU64,
    cold_active: AtomicUsize,
    cold_queued: AtomicUsize,
    cold_rejected: AtomicU64,
    cold_reserved_bytes: AtomicUsize,
    cold_queue_wait_ms: AtomicU64,
    cold_completed: AtomicU64,
    cold_elapsed_ms: AtomicU64,
    flight_active: AtomicUsize,
    flight_joined: AtomicU64,
    sidecar_loaded: AtomicU64,
    sidecar_missing: AtomicU64,
    sidecar_stale: AtomicU64,
    sidecar_corrupt: AtomicU64,
    fallback_builds: AtomicU64,
    repair_succeeded: AtomicU64,
    repair_failed: AtomicU64,
}

impl SearchRuntimeMetrics {
    fn snapshot(&self) -> SearchRuntimeMetricsSnapshot {
        SearchRuntimeMetricsSnapshot {
            scorer_active: self.scorer_active.load(Ordering::Relaxed),
            scorer_peak_active: self.scorer_peak_active.load(Ordering::Relaxed),
            scorer_queued: self.scorer_queued.load(Ordering::Relaxed),
            scorer_rejected: self.scorer_rejected.load(Ordering::Relaxed),
            scorer_queue_wait_ms: self.scorer_queue_wait_ms.load(Ordering::Relaxed),
            scorer_scratch_bytes: self.scorer_scratch_bytes.load(Ordering::Relaxed),
            scorer_peak_scratch_bytes: self.scorer_peak_scratch_bytes.load(Ordering::Relaxed),
            scorer_peak_per_query_scratch_bytes: self
                .scorer_peak_per_query_scratch_bytes
                .load(Ordering::Relaxed),
            score_completed: self.score_completed.load(Ordering::Relaxed),
            score_elapsed_ms: self.score_elapsed_ms.load(Ordering::Relaxed),
            cold_active: self.cold_active.load(Ordering::Relaxed),
            cold_queued: self.cold_queued.load(Ordering::Relaxed),
            cold_rejected: self.cold_rejected.load(Ordering::Relaxed),
            cold_reserved_bytes: self.cold_reserved_bytes.load(Ordering::Relaxed),
            cold_queue_wait_ms: self.cold_queue_wait_ms.load(Ordering::Relaxed),
            cold_completed: self.cold_completed.load(Ordering::Relaxed),
            cold_elapsed_ms: self.cold_elapsed_ms.load(Ordering::Relaxed),
            flight_active: self.flight_active.load(Ordering::Relaxed),
            flight_joined: self.flight_joined.load(Ordering::Relaxed),
            sidecar_loaded: self.sidecar_loaded.load(Ordering::Relaxed),
            sidecar_missing: self.sidecar_missing.load(Ordering::Relaxed),
            sidecar_stale: self.sidecar_stale.load(Ordering::Relaxed),
            sidecar_corrupt: self.sidecar_corrupt.load(Ordering::Relaxed),
            fallback_builds: self.fallback_builds.load(Ordering::Relaxed),
            repair_succeeded: self.repair_succeeded.load(Ordering::Relaxed),
            repair_failed: self.repair_failed.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone)]
struct SearchRuntime {
    scorer_lane: Arc<Semaphore>,
    scorer_queue_timeout: Duration,
    cold_lane: Arc<Semaphore>,
    cold_bytes: Arc<Semaphore>,
    cold_max_units: u32,
    cold_queue_timeout: Duration,
    metrics: Arc<SearchRuntimeMetrics>,
}

impl SearchRuntime {
    fn from_env() -> Self {
        let logical_cpus = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        let scorer_count = env_usize("CIH_SEARCH_SCORE_MAX_CONCURRENT", logical_cpus.clamp(1, 4));
        let cold_count = env_usize("CIH_SEARCH_COLD_MAX_CONCURRENT", 1);
        let cold_max_bytes = env_usize("CIH_SEARCH_COLD_MAX_BYTES", 512 * MIB);
        let cold_max_units = u32::try_from(cold_max_bytes.div_ceil(MIB))
            .unwrap_or(u32::MAX)
            .max(1);
        Self {
            scorer_lane: Arc::new(Semaphore::new(scorer_count)),
            scorer_queue_timeout: Duration::from_millis(env_u64(
                "CIH_SEARCH_SCORE_QUEUE_TIMEOUT_MS",
                2_000,
            )),
            cold_lane: Arc::new(Semaphore::new(cold_count)),
            cold_bytes: Arc::new(Semaphore::new(cold_max_units as usize)),
            cold_max_units,
            cold_queue_timeout: Duration::from_secs(env_u64(
                "CIH_SEARCH_COLD_QUEUE_TIMEOUT_SECS",
                5,
            )),
            metrics: Arc::new(SearchRuntimeMetrics::default()),
        }
    }

    async fn score(
        &self,
        index: Arc<SearchIndex>,
        query: String,
        limit: usize,
    ) -> Result<Vec<SearchHit>, SearchProviderError> {
        let started = Instant::now();
        self.metrics.scorer_queued.fetch_add(1, Ordering::Relaxed);
        let permit = tokio::time::timeout(
            self.scorer_queue_timeout,
            self.scorer_lane.clone().acquire_owned(),
        )
        .await;
        self.metrics.scorer_queued.fetch_sub(1, Ordering::Relaxed);
        self.metrics
            .scorer_queue_wait_ms
            .fetch_add(elapsed_ms(started), Ordering::Relaxed);
        let permit = match permit {
            Ok(Ok(permit)) => permit,
            Ok(Err(error)) => {
                return Err(SearchProviderError::new(
                    format!("search scoring lane closed: {error}"),
                    true,
                ))
            }
            Err(_) => {
                self.metrics.scorer_rejected.fetch_add(1, Ordering::Relaxed);
                return Err(SearchProviderError::new(
                    "search scoring capacity saturated; retry shortly",
                    true,
                ));
            }
        };
        let metrics = self.metrics.clone();
        let scratch_bytes = index.len().saturating_mul(std::mem::size_of::<f32>());
        run_blocking(blocking_timeout(), "search scoring", move || {
            let _active = ActiveGuard::scorer(metrics.clone(), scratch_bytes);
            let _permit = permit;
            let started = Instant::now();
            let hits = index.search(&query, limit);
            metrics.score_completed.fetch_add(1, Ordering::Relaxed);
            metrics
                .score_elapsed_ms
                .fetch_add(elapsed_ms(started), Ordering::Relaxed);
            hits
        })
        .await
        .map_err(|error| blocking_error("search scoring", error))
    }

    async fn run_cold<T, F>(
        &self,
        estimated_bytes: u64,
        operation: &'static str,
        work: F,
    ) -> Result<T, SearchProviderError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, SearchProviderError> + Send + 'static,
    {
        let requested_units = u32::try_from(estimated_bytes.div_ceil(MIB as u64))
            .unwrap_or(u32::MAX)
            .max(1);
        if requested_units > self.cold_max_units {
            self.metrics.cold_rejected.fetch_add(1, Ordering::Relaxed);
            return Err(SearchProviderError::new(
                format!(
                    "search cold-memory capacity saturated: estimated {} bytes exceeds {} bytes",
                    estimated_bytes,
                    u64::from(self.cold_max_units) * MIB as u64
                ),
                true,
            ));
        }

        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + self.cold_queue_timeout;
        self.metrics.cold_queued.fetch_add(1, Ordering::Relaxed);
        let count_permit =
            tokio::time::timeout_at(deadline, self.cold_lane.clone().acquire_owned()).await;
        let count_permit = match count_permit {
            Ok(Ok(permit)) => permit,
            Ok(Err(error)) => {
                self.metrics.cold_queued.fetch_sub(1, Ordering::Relaxed);
                return Err(SearchProviderError::new(
                    format!("search cold-load lane closed: {error}"),
                    true,
                ));
            }
            Err(_) => {
                self.metrics.cold_queued.fetch_sub(1, Ordering::Relaxed);
                self.metrics.cold_rejected.fetch_add(1, Ordering::Relaxed);
                return Err(SearchProviderError::new(
                    "search cold-load capacity saturated; retry shortly",
                    true,
                ));
            }
        };
        let bytes_permit = tokio::time::timeout_at(
            deadline,
            self.cold_bytes.clone().acquire_many_owned(requested_units),
        )
        .await;
        self.metrics.cold_queued.fetch_sub(1, Ordering::Relaxed);
        self.metrics
            .cold_queue_wait_ms
            .fetch_add(elapsed_ms(started), Ordering::Relaxed);
        let bytes_permit = match bytes_permit {
            Ok(Ok(permit)) => permit,
            Ok(Err(error)) => {
                return Err(SearchProviderError::new(
                    format!("search cold-memory lane closed: {error}"),
                    true,
                ))
            }
            Err(_) => {
                self.metrics.cold_rejected.fetch_add(1, Ordering::Relaxed);
                return Err(SearchProviderError::new(
                    "search cold-memory capacity saturated; retry shortly",
                    true,
                ));
            }
        };
        let reserved_bytes = requested_units as usize * MIB;
        let metrics = self.metrics.clone();
        run_blocking_heavy(blocking_timeout(), operation, move || {
            let _count_permit = count_permit;
            let _bytes_permit = bytes_permit;
            let _active = ActiveGuard::cold(metrics.clone(), reserved_bytes);
            let started = Instant::now();
            let result = work();
            metrics.cold_completed.fetch_add(1, Ordering::Relaxed);
            metrics
                .cold_elapsed_ms
                .fetch_add(elapsed_ms(started), Ordering::Relaxed);
            result
        })
        .await
        .map_err(|error| blocking_error(operation, error))?
    }
}

struct ActiveGuard {
    metrics: Arc<SearchRuntimeMetrics>,
    kind: ActiveKind,
}

enum ActiveKind {
    Scorer(usize),
    Cold(usize),
}

impl ActiveGuard {
    fn scorer(metrics: Arc<SearchRuntimeMetrics>, scratch_bytes: usize) -> Self {
        let active = metrics
            .scorer_active
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let aggregate_scratch = metrics
            .scorer_scratch_bytes
            .fetch_add(scratch_bytes, Ordering::Relaxed)
            .saturating_add(scratch_bytes);
        metrics
            .scorer_peak_active
            .fetch_max(active, Ordering::Relaxed);
        metrics
            .scorer_peak_scratch_bytes
            .fetch_max(aggregate_scratch, Ordering::Relaxed);
        metrics
            .scorer_peak_per_query_scratch_bytes
            .fetch_max(scratch_bytes, Ordering::Relaxed);
        Self {
            metrics,
            kind: ActiveKind::Scorer(scratch_bytes),
        }
    }

    fn cold(metrics: Arc<SearchRuntimeMetrics>, bytes: usize) -> Self {
        metrics.cold_active.fetch_add(1, Ordering::Relaxed);
        metrics
            .cold_reserved_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        Self {
            metrics,
            kind: ActiveKind::Cold(bytes),
        }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        match self.kind {
            ActiveKind::Scorer(scratch_bytes) => {
                self.metrics.scorer_active.fetch_sub(1, Ordering::Relaxed);
                self.metrics
                    .scorer_scratch_bytes
                    .fetch_sub(scratch_bytes, Ordering::Relaxed);
            }
            ActiveKind::Cold(bytes) => {
                self.metrics.cold_active.fetch_sub(1, Ordering::Relaxed);
                self.metrics
                    .cold_reserved_bytes
                    .fetch_sub(bytes, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct SearchCache {
    indexes: Arc<AsyncWeightedCache<SearchIndexKey, SearchIndex>>,
    flights: SearchFlights,
    runtime: SearchRuntime,
    store: Arc<dyn SearchIndexStore>,
    max_weight_bytes: usize,
    warned_oversize: Arc<StdMutex<HashSet<SearchIndexKey>>>,
    observed_indexes: Arc<StdMutex<HashMap<String, SearchIndexMetricsSnapshot>>>,
}

impl SearchCache {
    pub(crate) fn new(max_entries: usize, max_weight_bytes: usize) -> Self {
        Self::with_dependencies(
            max_entries,
            max_weight_bytes,
            SearchRuntime::from_env(),
            Arc::new(FileSearchIndexStore),
        )
    }

    fn with_dependencies(
        max_entries: usize,
        max_weight_bytes: usize,
        runtime: SearchRuntime,
        store: Arc<dyn SearchIndexStore>,
    ) -> Self {
        let metrics = runtime.metrics.clone();
        Self {
            indexes: Arc::new(AsyncWeightedCache::new(max_entries, max_weight_bytes)),
            flights: SearchFlights::new(metrics),
            runtime,
            store,
            max_weight_bytes,
            warned_oversize: Arc::new(StdMutex::new(HashSet::new())),
            observed_indexes: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    pub(crate) fn from_env() -> Self {
        Self::new(
            env_usize("CIH_SEARCH_CACHE_MAX_ENTRIES", 32),
            env_usize(
                "CIH_SEARCH_CACHE_MAX_BYTES",
                crate::config::DEFAULT_SEARCH_CACHE_MAX_BYTES,
            ),
        )
    }

    async fn get(&self, key: &SearchIndexKey) -> Option<Arc<SearchIndex>> {
        self.indexes.get_if(key, |_| true).await
    }

    async fn insert(&self, key: SearchIndexKey, index: Arc<SearchIndex>) {
        let weight = index.estimated_size_bytes();
        let documents = index.len();
        let result = self.indexes.insert(key.clone(), index, weight).await;
        self.record_index_metric(&key, documents, weight, result.retained);
        let metrics = self.metrics().await;
        if !result.retained {
            let mut warned = self
                .warned_oversize
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if warned.len() >= WARNING_KEY_LIMIT {
                warned.clear();
            }
            if warned.insert(key.clone()) {
                tracing::warn!(
                    artifact_version = %key.artifact_version,
                    weight_bytes = weight,
                    configured_bytes = self.max_weight_bytes,
                    remediation = "raise CIH_SEARCH_CACHE_MAX_BYTES and CIH_CACHE_MAX_BYTES after checking the container memory limit",
                    "search index exceeds retention budget; current waiters share it but a later request must reload the sidecar"
                );
            }
        }
        tracing::debug!(
            retained = result.retained,
            weight_bytes = weight,
            cache_hits = metrics.hits,
            cache_misses = metrics.misses,
            cache_builds = metrics.builds,
            cache_entries = metrics.retained_entries,
            cache_weight_bytes = metrics.retained_weight_bytes,
            cache_evictions = metrics.evictions,
            cache_oversize = metrics.oversize,
            "search cache updated"
        );
    }

    async fn get_or_load<F, Fut>(
        &self,
        key: SearchIndexKey,
        load: F,
    ) -> Result<Arc<SearchIndex>, SearchProviderError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Arc<SearchIndex>, SearchProviderError>>,
    {
        if let Some(index) = self.get(&key).await {
            return Ok(index);
        }
        let cache = self.clone();
        let flight_key = key.clone();
        self.flights
            .coalesce(key, move || async move {
                if let Some(index) = cache.get(&flight_key).await {
                    return Ok(index);
                }
                let index = load().await?;
                cache.insert(flight_key, index.clone()).await;
                Ok(index)
            })
            .await
    }

    pub(crate) async fn metrics(&self) -> AsyncCacheMetrics {
        self.indexes.metrics().await
    }

    pub(crate) fn runtime_metrics(&self) -> SearchRuntimeMetricsSnapshot {
        self.runtime.metrics.snapshot()
    }

    pub(crate) fn index_metrics(&self) -> Vec<SearchIndexMetricsSnapshot> {
        let values = self
            .observed_indexes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut metrics: Vec<_> = values.values().cloned().collect();
        metrics.sort_by(|left, right| {
            left.repository_id
                .cmp(&right.repository_id)
                .then_with(|| left.artifact_version.cmp(&right.artifact_version))
        });
        metrics
    }

    fn record_index_metric(
        &self,
        key: &SearchIndexKey,
        documents: usize,
        index_bytes: usize,
        retained: bool,
    ) {
        const MAX_OBSERVED_INDEXES: usize = 64;
        let repository_id = repository_metric_id(key);
        let metric_key = format!("{repository_id}:{}", key.artifact_version);
        let mut values = self
            .observed_indexes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if values.len() >= MAX_OBSERVED_INDEXES && !values.contains_key(&metric_key) {
            if let Some(oldest) = values.keys().min().cloned() {
                values.remove(&oldest);
            }
        }
        values.insert(
            metric_key,
            SearchIndexMetricsSnapshot {
                repository_id,
                artifact_version: key.artifact_version.clone(),
                documents,
                index_bytes,
                retained,
            },
        );
    }
}

fn repository_metric_id(key: &SearchIndexKey) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(key.artifacts_root.to_string_lossy().as_bytes());
    hasher.finalize().to_hex()[..16].to_string()
}

#[derive(Clone)]
struct SearchFlights {
    inner: Arc<SearchFlightsInner>,
}

struct SearchFlightsInner {
    values: StdMutex<HashMap<SearchIndexKey, Arc<SearchFlight>>>,
    metrics: Arc<SearchRuntimeMetrics>,
}

enum SearchFlightStatus {
    Running,
    Complete(Result<Arc<SearchIndex>, SearchProviderError>),
    Abandoned,
}

struct SearchFlightState {
    participants: usize,
    status: SearchFlightStatus,
}

struct SearchFlight {
    state: StdMutex<SearchFlightState>,
    completed: Notify,
}

impl SearchFlights {
    fn new(metrics: Arc<SearchRuntimeMetrics>) -> Self {
        Self {
            inner: Arc::new(SearchFlightsInner {
                values: StdMutex::new(HashMap::new()),
                metrics,
            }),
        }
    }

    async fn coalesce<F, Fut>(
        &self,
        key: SearchIndexKey,
        initialize: F,
    ) -> Result<Arc<SearchIndex>, SearchProviderError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Arc<SearchIndex>, SearchProviderError>>,
    {
        let mut initialize = Some(initialize);
        loop {
            let (flight, leader) = self.join(&key);
            let participant = FlightParticipant {
                owner: self.clone(),
                key: key.clone(),
                flight: flight.clone(),
            };
            if leader {
                let mut leader_guard = FlightLeaderGuard {
                    flight: flight.clone(),
                    metrics: self.inner.metrics.clone(),
                    published: false,
                };
                let result = initialize
                    .take()
                    .expect("search flight initializer consumed once")(
                )
                .await;
                {
                    let mut state = flight
                        .state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    state.status = SearchFlightStatus::Complete(result.clone());
                }
                leader_guard.published = true;
                self.inner
                    .metrics
                    .flight_active
                    .fetch_sub(1, Ordering::Relaxed);
                flight.completed.notify_waiters();
                drop(participant);
                return result;
            }

            loop {
                let notified = flight.completed.notified();
                let observed = {
                    let state = flight
                        .state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    match &state.status {
                        SearchFlightStatus::Running => None,
                        SearchFlightStatus::Complete(result) => Some(Ok(result.clone())),
                        SearchFlightStatus::Abandoned => Some(Err(())),
                    }
                };
                match observed {
                    Some(Ok(result)) => {
                        drop(participant);
                        return result;
                    }
                    Some(Err(())) => {
                        drop(participant);
                        break;
                    }
                    None => notified.await,
                }
            }
        }
    }

    fn join(&self, key: &SearchIndexKey) -> (Arc<SearchFlight>, bool) {
        let mut values = self
            .inner
            .values
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(existing) = values.get(key).cloned() {
            let mut state = existing
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !matches!(state.status, SearchFlightStatus::Abandoned) {
                state.participants += 1;
                self.inner
                    .metrics
                    .flight_joined
                    .fetch_add(1, Ordering::Relaxed);
                drop(state);
                return (existing, false);
            }
        }
        let flight = Arc::new(SearchFlight {
            state: StdMutex::new(SearchFlightState {
                participants: 1,
                status: SearchFlightStatus::Running,
            }),
            completed: Notify::new(),
        });
        values.insert(key.clone(), flight.clone());
        self.inner
            .metrics
            .flight_active
            .fetch_add(1, Ordering::Relaxed);
        (flight, true)
    }

    fn finish(&self, key: &SearchIndexKey, flight: &Arc<SearchFlight>) {
        let remove = {
            let mut state = flight
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.participants = state.participants.saturating_sub(1);
            state.participants == 0
        };
        if !remove {
            return;
        }
        let mut values = self
            .inner
            .values
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if values
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, flight))
        {
            values.remove(key);
        }
    }
}

struct FlightParticipant {
    owner: SearchFlights,
    key: SearchIndexKey,
    flight: Arc<SearchFlight>,
}

impl Drop for FlightParticipant {
    fn drop(&mut self) {
        self.owner.finish(&self.key, &self.flight);
    }
}

struct FlightLeaderGuard {
    flight: Arc<SearchFlight>,
    metrics: Arc<SearchRuntimeMetrics>,
    published: bool,
}

impl Drop for FlightLeaderGuard {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        {
            let mut state = self
                .flight
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.status = SearchFlightStatus::Abandoned;
        }
        self.metrics.flight_active.fetch_sub(1, Ordering::Relaxed);
        self.flight.completed.notify_waiters();
    }
}

#[derive(Clone)]
pub struct SearchState {
    cache: SearchCache,
    embed_store: Option<Arc<EmbedStore>>,
    artifacts_dir: Option<PathBuf>,
}

impl SearchState {
    #[cfg(test)]
    pub(crate) fn new(
        artifacts_dir: Option<PathBuf>,
        embed_store: Option<Arc<EmbedStore>>,
    ) -> Self {
        Self::with_cache(artifacts_dir, embed_store, SearchCache::from_env())
    }

    pub(crate) fn with_cache(
        artifacts_dir: Option<PathBuf>,
        embed_store: Option<Arc<EmbedStore>>,
        cache: SearchCache,
    ) -> Self {
        Self {
            cache,
            embed_store,
            artifacts_dir,
        }
    }

    pub(crate) async fn query_hits(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>, SearchProviderError> {
        if self.artifacts_dir.is_none() && self.embed_store.is_none() {
            return Err(SearchProviderError::new(
                "query unavailable: set CIH_ARTIFACTS_DIR for BM25 and/or CIH_PG_URL for semantic search",
                false,
            ));
        }

        let lexical_query = query.to_string();
        let semantic_query = query.to_string();
        let lexical = async {
            match self.bm25_index().await? {
                Some(index) => {
                    self.cache
                        .runtime
                        .score(index, lexical_query, limit.saturating_mul(2))
                        .await
                }
                None => Ok(Vec::new()),
            }
        };
        let semantic = async {
            let Some(embed_store) = &self.embed_store else {
                return Ok(Vec::new());
            };
            embed_store
                .semantic_search(&semantic_query, limit.saturating_mul(2), 0.5)
                .await
                .map(|hits| hits.into_iter().map(semantic_to_search_hit).collect())
                .map_err(|error| {
                    SearchProviderError::new(format!("semantic search failed: {error}"), true)
                })
        };
        let (lexical, semantic) = tokio::join!(lexical, semantic);
        Ok(rrf_merge(lexical?, semantic?, limit))
    }

    async fn bm25_index(&self) -> Result<Option<Arc<SearchIndex>>, SearchProviderError> {
        let Some(artifacts_root) = &self.artifacts_dir else {
            return Ok(None);
        };
        let artifacts =
            cih_core::GraphArtifacts::latest_in_dir(artifacts_root).map_err(|error| {
                SearchProviderError::new(
                    format!("search artifact resolution failed: {error}"),
                    true,
                )
            })?;
        let canonical_root = canonical_or_absolute(artifacts_root);
        let key = SearchIndexKey {
            artifacts_root: canonical_root,
            artifact_version: artifacts.version.to_string(),
            schema: search_schema_fingerprint(),
        };
        let cache = self.cache.clone();
        self.cache
            .get_or_load(
                key,
                move || async move { cache.load_index(artifacts).await },
            )
            .await
            .map(Some)
    }
}

impl SearchCache {
    async fn load_index(
        &self,
        artifacts: cih_core::GraphArtifacts,
    ) -> Result<Arc<SearchIndex>, SearchProviderError> {
        let source = SearchIndexSource::from_nodes_file(
            &artifacts.nodes_path,
            artifacts.version.to_string(),
        )
        .map_err(|error| {
            SearchProviderError::new(format!("search source identity failed: {error}"), true)
        })?;
        let artifacts_dir = artifacts
            .nodes_path
            .parent()
            .ok_or_else(|| SearchProviderError::new("invalid search artifact path", false))?
            .to_path_buf();
        let sidecar_path = search_index_path(&artifacts_dir);
        let sidecar_enabled = env_bool("CIH_SEARCH_SIDECAR_ENABLED", true);
        let fallback_reservation = source.nodes_len.saturating_mul(2);
        let (estimated_bytes, load_sidecar) = if sidecar_enabled {
            match self.store.inspect(&sidecar_path) {
                Ok(SearchIndexInspection::Present(metadata)) => {
                    match sidecar_reservation_bytes(&metadata) {
                        Some(bytes) => (bytes, true),
                        None => {
                            self.runtime
                                .metrics
                                .sidecar_corrupt
                                .fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(
                                "search sidecar header has an impossible size ratio; rebuilding"
                            );
                            (fallback_reservation, false)
                        }
                    }
                }
                Ok(SearchIndexInspection::Missing) => (fallback_reservation, true),
                Ok(SearchIndexInspection::Invalid(reason)) => {
                    self.runtime
                        .metrics
                        .sidecar_corrupt
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(reason, "search sidecar header invalid; rebuilding");
                    (fallback_reservation, false)
                }
                Err(error) => {
                    self.runtime
                        .metrics
                        .sidecar_corrupt
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(error = %error, "search sidecar header unreadable; rebuilding");
                    (fallback_reservation, false)
                }
            }
        } else {
            (fallback_reservation, false)
        };
        let store = self.store.clone();
        let metrics = self.runtime.metrics.clone();
        self.runtime
            .run_cold(estimated_bytes, "search index cold load", move || {
                if load_sidecar {
                    match store.load(&sidecar_path, &source) {
                        Ok(SearchIndexLoad::Loaded { index, metadata }) => {
                            metrics.sidecar_loaded.fetch_add(1, Ordering::Relaxed);
                            tracing::info!(
                                artifact_version = %source.artifact_version,
                                index_bytes = metadata.retained_size_bytes,
                                payload_bytes = metadata.payload_len,
                                source = "sidecar",
                                "search index loaded"
                            );
                            return Ok(Arc::from(index));
                        }
                        Ok(SearchIndexLoad::Missing) => {
                            metrics.sidecar_missing.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(SearchIndexLoad::Stale(reason)) => {
                            metrics.sidecar_stale.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(reason, "search sidecar stale; rebuilding");
                        }
                        Ok(SearchIndexLoad::Corrupt(reason)) => {
                            metrics.sidecar_corrupt.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(reason, "search sidecar corrupt; rebuilding");
                        }
                        Err(error) => {
                            metrics.sidecar_corrupt.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(error = %error, "search sidecar unreadable; rebuilding");
                        }
                    }
                }

                metrics.fallback_builds.fetch_add(1, Ordering::Relaxed);
                let nodes = artifacts.stream_nodes().map_err(|error| {
                    SearchProviderError::new(
                        format!("search index build could not open nodes: {error}"),
                        true,
                    )
                })?;
                let index = SearchIndex::try_build(nodes).map_err(|error| {
                    SearchProviderError::new(format!("search index build failed: {error}"), true)
                })?;
                if sidecar_enabled {
                    match store.persist(&sidecar_path, &source, &index) {
                        Ok(metadata) => {
                            metrics.repair_succeeded.fetch_add(1, Ordering::Relaxed);
                            tracing::info!(
                                artifact_version = %source.artifact_version,
                                index_bytes = metadata.retained_size_bytes,
                                payload_bytes = metadata.payload_len,
                                "search sidecar repaired"
                            );
                        }
                        Err(error) => {
                            metrics.repair_failed.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(
                                failure = error.failure.label(),
                                "search sidecar repair failed; serving the in-memory index; generate it during analyze before mounting read-only artifacts"
                            );
                        }
                    }
                }
                Ok(Arc::new(index))
            })
            .await
    }
}

fn sidecar_reservation_bytes(metadata: &cih_search::SearchIndexMetadata) -> Option<u64> {
    if metadata.payload_len == 0 || metadata.retained_size_bytes == 0 {
        return None;
    }
    let larger = metadata.payload_len.max(metadata.retained_size_bytes);
    let smaller = metadata.payload_len.min(metadata.retained_size_bytes);
    if larger > smaller.saturating_mul(8).saturating_add(MIB as u64) {
        return None;
    }
    metadata
        .payload_len
        .checked_mul(2)
        .map(|payload| payload.max(metadata.retained_size_bytes.saturating_mul(5) / 4))
}

#[async_trait]
impl SearchProvider for SearchState {
    async fn query_hits(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>, SearchProviderError> {
        SearchState::query_hits(self, query, limit).await
    }
}

#[doc(hidden)]
pub fn query_limit(raw: usize) -> usize {
    if raw == 0 { 10 } else { raw }.clamp(1, 50)
}

fn semantic_to_search_hit(hit: SemanticHit) -> SearchHit {
    SearchHit::from_parts(
        hit.node_id,
        hit.kind,
        hit.name,
        None,
        hit.file,
        hit.range,
        hit.score,
        "semantic",
    )
}

fn canonical_or_absolute(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|current| current.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

fn blocking_error(operation: &'static str, error: BlockingError) -> SearchProviderError {
    match error {
        BlockingError::TimedOut { .. } => SearchProviderError::new(
            format!("{operation} timed out; retry after current work drains"),
            true,
        ),
        BlockingError::Saturated { .. } => SearchProviderError::new(
            format!("{operation} capacity saturated; retry shortly"),
            true,
        ),
        BlockingError::Panicked { detail, .. } => {
            SearchProviderError::new(format!("{operation} failed internally: {detail}"), false)
        }
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(default)
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{Node, NodeId, NodeKind, Range, VersionId};
    use std::sync::atomic::AtomicUsize;

    fn key(version: &str) -> SearchIndexKey {
        SearchIndexKey {
            artifacts_root: PathBuf::from("/repo/.cih/artifacts"),
            artifact_version: version.into(),
            schema: search_schema_fingerprint(),
        }
    }

    #[tokio::test]
    async fn repository_states_share_one_weighted_search_cache() {
        let cache = SearchCache::new(8, 1024 * 1024);
        let state_a = SearchState::with_cache(None, None, cache.clone());
        let state_b = SearchState::with_cache(None, None, cache.clone());
        let index = Arc::new(SearchIndex::default());
        cache.insert(key("v1"), index.clone()).await;

        let from_b = state_b.cache.get(&key("v1")).await.unwrap();
        assert!(Arc::ptr_eq(&index, &from_b));
        assert!(Arc::ptr_eq(
            &state_a.cache.get(&key("v1")).await.unwrap(),
            &from_b
        ));
        assert_eq!(cache.metrics().await.retained_entries, 1);
    }

    #[tokio::test]
    async fn oversize_search_index_is_served_but_not_retained() {
        let cache = SearchCache::new(8, 1);
        cache
            .insert(key("v1"), Arc::new(SearchIndex::default()))
            .await;
        assert!(cache.get(&key("v1")).await.is_none());
        let metrics = cache.metrics().await;
        assert_eq!(metrics.retained_entries, 0);
        assert_eq!(metrics.oversize, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oversize_cold_burst_shares_one_result() {
        let cache = Arc::new(SearchCache::new(8, 1));
        let loads = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let cache = cache.clone();
            let loads = loads.clone();
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_load(key("v1"), || async move {
                        loads.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok(Arc::new(SearchIndex::default()))
                    })
                    .await
                    .unwrap()
            }));
        }
        let mut indexes = Vec::new();
        for task in tasks {
            indexes.push(task.await.unwrap());
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert!(indexes
            .iter()
            .skip(1)
            .all(|index| Arc::ptr_eq(&indexes[0], index)));
        assert_eq!(cache.metrics().await.retained_entries, 0);
    }

    #[tokio::test]
    async fn missing_sidecar_is_repaired_and_reused_after_restart() {
        let directory = std::env::temp_dir().join(format!(
            "cih-server-search-sidecar-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let version_dir = directory.join("v1");
        let node = Node {
            id: NodeId::new("Method:com.acme.OwnerService#findAll/0"),
            kind: NodeKind::Method,
            name: "findAll".into(),
            qualified_name: Some("com.acme.OwnerService.findAll".into()),
            file: "src/OwnerService.java".into(),
            range: Range::default(),
            props: None,
        };
        cih_core::GraphArtifacts::write(&version_dir, VersionId::new("v1"), &[node], &[]).unwrap();

        let first_cache = SearchCache::new(8, 8 * MIB);
        let first = SearchState::with_cache(Some(directory.clone()), None, first_cache.clone());
        let hits = first.query_hits("owner service", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(search_index_path(&version_dir).is_file());
        assert_eq!(first_cache.runtime_metrics().fallback_builds, 1);
        assert_eq!(first_cache.runtime_metrics().repair_succeeded, 1);

        let second_cache = SearchCache::new(8, 8 * MIB);
        let second = SearchState::with_cache(Some(directory.clone()), None, second_cache.clone());
        let hits = second.query_hits("owner service", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(second_cache.runtime_metrics().sidecar_loaded, 1);
        assert_eq!(second_cache.runtime_metrics().fallback_builds, 0);
        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn scorer_guard_retains_scratch_high_water_marks() {
        let metrics = Arc::new(SearchRuntimeMetrics::default());
        let first = ActiveGuard::scorer(metrics.clone(), 128);
        let second = ActiveGuard::scorer(metrics.clone(), 256);

        let active = metrics.snapshot();
        assert_eq!(active.scorer_active, 2);
        assert_eq!(active.scorer_scratch_bytes, 384);
        assert_eq!(active.scorer_peak_active, 2);
        assert_eq!(active.scorer_peak_scratch_bytes, 384);
        assert_eq!(active.scorer_peak_per_query_scratch_bytes, 256);

        drop(second);
        drop(first);
        let idle = metrics.snapshot();
        assert_eq!(idle.scorer_active, 0);
        assert_eq!(idle.scorer_scratch_bytes, 0);
        assert_eq!(idle.scorer_peak_active, 2);
        assert_eq!(idle.scorer_peak_scratch_bytes, 384);
        assert_eq!(idle.scorer_peak_per_query_scratch_bytes, 256);
    }
}
