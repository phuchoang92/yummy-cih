//! Local adapters for persistence, caching, processes, and repository access.

pub(crate) mod artifact_repository;
pub(crate) mod cache;
pub(crate) mod git_changed_files;
pub(crate) mod graph_store_provider;
pub(crate) mod index_jobs;
pub(crate) mod jsonl_page_index;
pub(crate) mod local_job_scheduler;
pub(crate) mod repo_context_provider;
pub(crate) mod search_provider;
pub(crate) mod wiki_repository;
