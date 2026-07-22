//! Local adapters for persistence, caching, processes, and repository access.

pub(crate) mod artifact_cross_repo_graph;
pub(crate) mod artifact_index_store;
pub(crate) mod artifact_repository;
pub(crate) mod cache;
pub(crate) mod engine_process_runner;
pub(crate) mod file_search_index_store;
pub(crate) mod git_changed_files;
pub(crate) mod graph_store_provider;
pub(crate) mod index_jobs;
pub(crate) mod jsonl_page_index;
pub(crate) mod local_job_scheduler;
pub(crate) mod repo_context_provider;
pub(crate) mod retrieval_metrics;
pub(crate) mod search_provider;
pub(crate) mod tracing_observability;
pub(crate) mod wiki_repository;
