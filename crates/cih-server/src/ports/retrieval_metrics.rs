use async_trait::async_trait;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub(crate) struct CacheMetricsSnapshot {
    pub(crate) requests: u64,
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) builds: u64,
    pub(crate) retained_entries: usize,
    pub(crate) retained_weight_bytes: usize,
    pub(crate) evictions: u64,
    pub(crate) oversize: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub(crate) struct SearchRuntimeMetricsSnapshot {
    pub(crate) scorer_active: usize,
    pub(crate) scorer_queued: usize,
    pub(crate) scorer_rejected: u64,
    pub(crate) scorer_queue_wait_ms: u64,
    pub(crate) scorer_scratch_bytes: usize,
    pub(crate) score_completed: u64,
    pub(crate) score_elapsed_ms: u64,
    pub(crate) cold_active: usize,
    pub(crate) cold_queued: usize,
    pub(crate) cold_rejected: u64,
    pub(crate) cold_reserved_bytes: usize,
    pub(crate) cold_queue_wait_ms: u64,
    pub(crate) cold_completed: u64,
    pub(crate) cold_elapsed_ms: u64,
    pub(crate) flight_active: usize,
    pub(crate) flight_joined: u64,
    pub(crate) sidecar_loaded: u64,
    pub(crate) sidecar_missing: u64,
    pub(crate) sidecar_stale: u64,
    pub(crate) sidecar_corrupt: u64,
    pub(crate) fallback_builds: u64,
    pub(crate) repair_succeeded: u64,
    pub(crate) repair_failed: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub(crate) struct GrepRuntimeMetricsSnapshot {
    pub(crate) active: usize,
    pub(crate) queued: usize,
    pub(crate) rejected: u64,
    pub(crate) requests: u64,
    pub(crate) partial: u64,
    pub(crate) deadline_partial: u64,
    pub(crate) queue_wait_ms: u64,
    pub(crate) elapsed_ms: u64,
    pub(crate) candidate_files: u64,
    pub(crate) files_scanned: u64,
    pub(crate) files_skipped: u64,
    pub(crate) matches_returned: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub(crate) struct WikiRuntimeMetricsSnapshot {
    pub(crate) manifest_overview_attempted: u64,
    pub(crate) manifest_overview_succeeded: u64,
    pub(crate) manifest_overview_failed: u64,
    pub(crate) live_build_attempted: u64,
    pub(crate) live_build_succeeded: u64,
    pub(crate) live_build_rejected_size: u64,
    pub(crate) live_build_failed: u64,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub(crate) struct SearchIndexMetricsSnapshot {
    pub(crate) repository_id: String,
    pub(crate) artifact_version: String,
    pub(crate) documents: usize,
    pub(crate) index_bytes: usize,
    pub(crate) retained: bool,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub(crate) struct RetrievalMetricsSnapshot {
    pub(crate) search_cache: CacheMetricsSnapshot,
    pub(crate) search_runtime: SearchRuntimeMetricsSnapshot,
    pub(crate) search_indexes: Vec<SearchIndexMetricsSnapshot>,
    pub(crate) wiki_cache: CacheMetricsSnapshot,
    pub(crate) wiki_runtime: WikiRuntimeMetricsSnapshot,
    pub(crate) grep: GrepRuntimeMetricsSnapshot,
}

#[async_trait]
pub(crate) trait RetrievalMetricsProvider: Send + Sync {
    async fn snapshot(&self) -> RetrievalMetricsSnapshot;
}
