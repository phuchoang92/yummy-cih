//! External boundaries consumed by application services.

pub(crate) mod artifact_index_store;
pub(crate) mod artifact_repository;
pub(crate) mod blocking_runtime;
pub(crate) mod changed_files_source;
pub(crate) mod cross_repo_graph_provider;
pub(crate) mod index_target_resolver;
pub(crate) mod job_scheduler;
pub(crate) mod observability;
pub(crate) mod process_runner;
pub(crate) mod repo_context_provider;
pub(crate) mod retrieval_metrics;
pub(crate) mod search_index_store;
pub(crate) mod search_provider;
pub(crate) mod wiki_materialization_store;
