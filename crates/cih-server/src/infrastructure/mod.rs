//! Local adapters for persistence, caching, processes, and repository access.

pub(crate) mod artifact_repository;
pub(crate) mod blocking_runtime;
pub(crate) mod cache;
pub(crate) mod cross_repo_graph;
pub(crate) mod index_jobs;
pub(crate) mod local_job_scheduler;
pub(crate) mod repo_context_provider;
pub(crate) mod search_provider;
pub(crate) mod wiki_repository;
