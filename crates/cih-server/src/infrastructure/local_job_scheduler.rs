//! Local bounded scheduler for engine indexing jobs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{watch, Mutex, Semaphore};

use crate::domain::error::AppError;
use crate::domain::indexing::{
    IndexJobSnapshot, IndexJobSpec, IndexQueueMetrics, IndexSchedulerReceipt, ResolvedRepoTarget,
};
use crate::infrastructure::engine_process_runner::TokioEngineProcessRunner;
use crate::infrastructure::index_jobs::{
    evict_terminal, find_engine_binary, new_job_id, unix_now_secs, JobState, Jobs,
};
use crate::ports::artifact_repository::ArtifactRepository;
use crate::ports::blocking_runtime::{blocking_timeout, run_blocking};
use crate::ports::index_target_resolver::IndexTargetResolver;
use crate::ports::job_scheduler::IndexJobScheduler;
use crate::ports::process_runner::{EngineProcessOutcome, EngineProcessRunner, EngineProcessSpec};

/// Admission-controlled runner for `cih-engine analyze` jobs (S5): a global
/// running-cap semaphore, a bounded admission queue, one active job per
/// canonical repo (duplicate submissions coalesce onto the active job), a hard
/// deadline that kills the child, and capped output capture. Registry
/// freshness after a successful job needs no explicit invalidation:
/// `Registry::load_cached` is mtime-checked and the engine's save bumps it.
#[derive(Clone)]
pub struct IndexScheduler {
    jobs: Jobs,
    /// Bounds concurrently *running* engine processes.
    running: Arc<Semaphore>,
    /// Canonical repo path → active (queued or running) job.
    active: Arc<Mutex<HashMap<PathBuf, ActiveJob>>>,
    /// Queued + running bound: running cap + queue capacity.
    admission_capacity: usize,
    /// Job id → cancellation signal for queued/running jobs (dropped when the
    /// job settles).
    cancels: Arc<Mutex<HashMap<String, watch::Sender<bool>>>>,
    job_timeout: Duration,
    output_cap: usize,
    artifacts: Arc<dyn ArtifactRepository>,
    process_runner: Arc<dyn EngineProcessRunner>,
    rejected: Arc<AtomicU64>,
    engine: IndexEngineConfig,
}

#[derive(Clone, Debug)]
struct IndexEngineConfig {
    backend: String,
    falkor_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct IndexCommandKey {
    graph_key: String,
    /// Sorted and deduplicated because language order does not change analysis.
    languages: Vec<String>,
}

impl IndexCommandKey {
    fn new(graph_key: String, mut languages: Vec<String>) -> Self {
        languages.sort();
        languages.dedup();
        Self {
            graph_key,
            languages,
        }
    }
}

#[derive(Clone, Debug)]
struct ActiveJob {
    job_id: String,
    command: IndexCommandKey,
}

impl IndexScheduler {
    /// Limits from env: `CIH_INDEX_MAX_CONCURRENT` (default 1),
    /// `CIH_INDEX_QUEUE_CAPACITY` (default 16), `CIH_INDEX_TIMEOUT_SECS`
    /// (default 1800, 0 disables), `CIH_INDEX_OUTPUT_CAP_BYTES` (default 1 MiB).
    pub(crate) fn new(
        jobs: Jobs,
        artifacts: Arc<dyn ArtifactRepository>,
        backend: String,
        falkor_url: String,
    ) -> Self {
        let usize_env = |name: &str, default: usize| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|&n| n > 0)
                .unwrap_or(default)
        };
        let timeout_secs = std::env::var("CIH_INDEX_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30 * 60);
        let timeout = if timeout_secs == 0 {
            Duration::from_secs(365 * 24 * 60 * 60)
        } else {
            Duration::from_secs(timeout_secs)
        };
        Self::with_limits(
            jobs,
            usize_env("CIH_INDEX_MAX_CONCURRENT", 1),
            usize_env("CIH_INDEX_QUEUE_CAPACITY", 16),
            timeout,
            usize_env("CIH_INDEX_OUTPUT_CAP_BYTES", 1024 * 1024),
            artifacts,
            IndexEngineConfig {
                backend,
                falkor_url,
            },
        )
    }

    fn with_limits(
        jobs: Jobs,
        max_concurrent: usize,
        queue_capacity: usize,
        job_timeout: Duration,
        output_cap: usize,
        artifacts: Arc<dyn ArtifactRepository>,
        engine: IndexEngineConfig,
    ) -> Self {
        Self::with_runner_limits(
            jobs,
            max_concurrent,
            queue_capacity,
            job_timeout,
            output_cap,
            artifacts,
            Arc::new(TokioEngineProcessRunner),
            engine,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn with_runner_limits(
        jobs: Jobs,
        max_concurrent: usize,
        queue_capacity: usize,
        job_timeout: Duration,
        output_cap: usize,
        artifacts: Arc<dyn ArtifactRepository>,
        process_runner: Arc<dyn EngineProcessRunner>,
        engine: IndexEngineConfig,
    ) -> Self {
        Self {
            jobs,
            running: Arc::new(Semaphore::new(max_concurrent)),
            active: Arc::new(Mutex::new(HashMap::new())),
            admission_capacity: max_concurrent + queue_capacity,
            cancels: Arc::new(Mutex::new(HashMap::new())),
            job_timeout,
            output_cap,
            artifacts,
            process_runner,
            rejected: Arc::new(AtomicU64::new(0)),
            engine,
        }
    }

    /// Signal cancellation for a queued or running job. The lifecycle task
    /// kills a running engine (`kill_on_drop`) and settles the job as
    /// `cancelled`; callers poll `index_status` for the final state.
    async fn signal_cancel(&self, job_id: &str) -> Result<(), AppError> {
        if let Some(cancel) = self.cancels.lock().await.get(job_id) {
            let _ = cancel.send(true);
            return Ok(());
        }
        match self.jobs.read().await.get(job_id) {
            Some(state) => Err(AppError::InvalidInput {
                field: "job_id",
                message: format!(
                    "job '{job_id}' already finished ({}) — nothing to cancel",
                    state.status_label()
                ),
            }),
            None => Err(AppError::NotFound {
                entity: "index job",
                key: job_id.to_string(),
            }),
        }
    }

    /// Terminal bookkeeping: record the state, release the repo, drop the
    /// cancel channel.
    async fn settle(&self, job_id: &str, canonical: &Path, state: JobState) {
        self.jobs.write().await.insert(job_id.to_string(), state);
        let mut active = self.active.lock().await;
        if active
            .get(canonical)
            .is_some_and(|active| active.job_id == job_id)
        {
            active.remove(canonical);
        }
        self.cancels.lock().await.remove(job_id);
    }

    /// Register the job (state `queued`, cancel channel) and spawn its
    /// lifecycle: wait for a running slot → run the engine → settle a terminal
    /// state. The caller has already claimed the repo via [`admit`](Self::admit).
    async fn spawn_job(&self, job_id: String, canonical: PathBuf, spec: EngineProcessSpec) {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        {
            let mut jobs = self.jobs.write().await;
            jobs.insert(
                job_id.clone(),
                JobState::Queued {
                    queued_at_secs: unix_now_secs(),
                },
            );
            evict_terminal(&mut jobs);
        }
        self.cancels.lock().await.insert(job_id.clone(), cancel_tx);
        let sched = self.clone();
        tokio::spawn(async move {
            // Queued phase: a cancel here must not wait for a running slot.
            let permit = tokio::select! {
                permit = sched.running.clone().acquire_owned() => match permit {
                    Ok(permit) => permit,
                    // The semaphore is never closed; if it somehow is, fail
                    // the job rather than hang it.
                    Err(closed) => {
                        let now = unix_now_secs();
                        sched
                            .settle(&job_id, &canonical, JobState::Failed {
                                started_at_secs: now,
                                finished_at_secs: now,
                                error: format!("scheduler unavailable: {closed}"),
                            })
                            .await;
                        return;
                    }
                },
                _ = wait_cancelled(cancel_rx.clone()) => {
                    sched
                        .settle(&job_id, &canonical, JobState::Cancelled {
                            cancelled_at_secs: unix_now_secs(),
                        })
                        .await;
                    return;
                }
            };
            let started_at_secs = unix_now_secs();
            sched
                .jobs
                .write()
                .await
                .insert(job_id.clone(), JobState::Running { started_at_secs });

            let outcome = sched.process_runner.run(spec, cancel_rx).await;
            let finished_at_secs = unix_now_secs();
            let state = match outcome {
                EngineProcessOutcome::Cancelled => JobState::Cancelled {
                    cancelled_at_secs: finished_at_secs,
                },
                EngineProcessOutcome::Exited {
                    success: true,
                    stdout,
                    truncated,
                    ..
                } => JobState::Done {
                    started_at_secs,
                    finished_at_secs,
                    output: stdout.trim().to_string(),
                    output_truncated: truncated,
                },
                EngineProcessOutcome::Exited {
                    code,
                    stdout,
                    stderr,
                    ..
                } => {
                    let stderr: String = stderr
                        .lines()
                        .filter(|l| !l.contains('\x1b'))
                        .collect::<Vec<_>>()
                        .join("\n");
                    JobState::Failed {
                        started_at_secs,
                        finished_at_secs,
                        error: format!(
                            "cih-engine exited {code}: {}\n{}",
                            stderr.trim(),
                            stdout.trim()
                        ),
                    }
                }
                EngineProcessOutcome::TimedOut => JobState::TimedOut {
                    started_at_secs,
                    finished_at_secs,
                    timeout_secs: sched.job_timeout.as_secs(),
                },
                EngineProcessOutcome::LaunchFailed(error) => JobState::Failed {
                    started_at_secs,
                    finished_at_secs,
                    error,
                },
            };
            if matches!(state, JobState::Done { .. }) {
                sched.artifacts.invalidate_repo(&canonical);
            }
            sched.settle(&job_id, &canonical, state).await;
            drop(permit);
        });
    }

    /// Admission decision under the active-jobs lock: identical commands
    /// coalesce, a different command for an active repo conflicts, a full queue
    /// rejects, and otherwise the repo is claimed.
    async fn admit(&self, canonical: &Path, job_id: &str, command: &IndexCommandKey) -> Admission {
        let mut active = self.active.lock().await;
        if let Some(existing) = active.get(canonical) {
            return if existing.command == *command {
                Admission::Duplicate(existing.job_id.clone())
            } else {
                Admission::Conflict {
                    existing: existing.job_id.clone(),
                }
            };
        }
        if active.len() >= self.admission_capacity {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            return Admission::QueueFull {
                active: active.len(),
                capacity: self.admission_capacity,
            };
        }
        active.insert(
            canonical.to_path_buf(),
            ActiveJob {
                job_id: job_id.to_string(),
                command: command.clone(),
            },
        );
        Admission::Admitted
    }
}

enum Admission {
    /// This repo already has a queued/running job — reuse its id.
    Duplicate(String),
    /// The repo is busy with a different indexing command.
    Conflict {
        existing: String,
    },
    QueueFull {
        active: usize,
        capacity: usize,
    },
    Admitted,
}

/// Resolve the graph key an index job for `canonical` must target: a
/// registered path always uses its own registry key, an unregistered path
/// requires an explicit new key, and a key owned by a different repo is
/// rejected. The server's primary key is never applied implicitly — doing so
/// loaded any `repo_path` into the primary graph.
fn resolve_target_graph_key(
    reg: &cih_core::Registry,
    canonical: &Path,
    explicit: &str,
) -> Result<String, String> {
    let explicit = explicit.trim();
    let owner = reg.entries.iter().find(|e| {
        Path::new(&e.path)
            .canonicalize()
            .map(|p| p == canonical)
            .unwrap_or_else(|_| Path::new(&e.path) == canonical)
    });
    match owner {
        Some(entry) => {
            if !explicit.is_empty() && explicit != entry.graph_key {
                return Err(format!(
                    "repo '{}' is registered under graph key '{}'; omit `graph_key` or pass \
                     that key (got '{explicit}')",
                    entry.name, entry.graph_key
                ));
            }
            Ok(entry.graph_key.clone())
        }
        None => {
            if explicit.is_empty() {
                return Err(
                    "repo is not in the registry; pass an explicit `graph_key` to index it \
                     under (a new key — not one owned by another repo)"
                        .to_string(),
                );
            }
            if let Some(other) = reg.entries.iter().find(|e| e.graph_key == explicit) {
                return Err(format!(
                    "graph key '{explicit}' is already owned by repo '{}' ({}); choose a new key",
                    other.name, other.path
                ));
            }
            Ok(explicit.to_string())
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct RegistryIndexTargetResolver;

fn resolve_index_target(
    repo_path: &str,
    requested_graph_key: &str,
) -> Result<ResolvedRepoTarget, AppError> {
    let repo = Path::new(repo_path);
    if !repo.is_dir() {
        return Err(AppError::InvalidInput {
            field: "repo_path",
            message: format!("'{repo_path}' does not exist or is not a directory"),
        });
    }
    let canonical = repo
        .canonicalize()
        .map_err(|error| AppError::InvalidInput {
            field: "repo_path",
            message: format!("cannot canonicalize repository path: {error}"),
        })?;
    // Fresh registry read: a just-finished index job may have added the entry
    // this resolution depends on.
    let graph_key =
        resolve_target_graph_key(&cih_core::Registry::load(), &canonical, requested_graph_key)
            .map_err(|message| AppError::InvalidInput {
                field: "graph_key",
                message,
            })?;
    Ok(ResolvedRepoTarget {
        canonical_path: canonical,
        graph_key,
    })
}

#[async_trait]
impl IndexTargetResolver for RegistryIndexTargetResolver {
    async fn resolve(
        &self,
        repo_path: &str,
        requested_graph_key: &str,
    ) -> Result<ResolvedRepoTarget, AppError> {
        let repo_path = repo_path.to_string();
        let requested_graph_key = requested_graph_key.to_string();
        run_blocking(blocking_timeout(), "index target resolution", move || {
            resolve_index_target(&repo_path, &requested_graph_key)
        })
        .await
        .map_err(|error| AppError::Unavailable {
            dependency: "index target resolver",
            message: error.to_string(),
            retryable: true,
        })?
    }
}

#[async_trait]
impl IndexJobScheduler for IndexScheduler {
    async fn submit(&self, spec: IndexJobSpec) -> Result<IndexSchedulerReceipt, AppError> {
        let canonical = spec.target.canonical_path;
        let command_key = IndexCommandKey::new(spec.target.graph_key, spec.languages);
        let job_id = new_job_id();
        match self.admit(&canonical, &job_id, &command_key).await {
            Admission::Duplicate(existing) => {
                return Ok(IndexSchedulerReceipt {
                    job_id: existing,
                    deduplicated: true,
                });
            }
            Admission::Conflict { existing } => {
                return Err(AppError::InvalidInput {
                    field: "repo_path",
                    message: format!(
                        "repo already has a different active index job '{existing}'; \
                         wait for it to finish or cancel it before changing graph_key/languages"
                    ),
                });
            }
            Admission::QueueFull { active, capacity } => {
                return Err(AppError::Unavailable {
                    dependency: "index queue",
                    message: format!(
                        "queue full ({active} active jobs, capacity {capacity}); \
                         retry after a job finishes"
                    ),
                    retryable: true,
                });
            }
            Admission::Admitted => {}
        }

        let mut args = vec![
            "analyze".to_string(),
            canonical.display().to_string(),
            "--all".to_string(),
        ];
        for language in &command_key.languages {
            args.push("--language".to_string());
            args.push(language.clone());
        }
        let spec = EngineProcessSpec {
            program: find_engine_binary(),
            args,
            current_dir: canonical.clone(),
            env: vec![
                ("CIH_GRAPH_BACKEND".into(), self.engine.backend.clone()),
                ("FALKOR_URL".into(), self.engine.falkor_url.clone()),
                ("CIH_GRAPH_KEY".into(), command_key.graph_key.clone()),
                ("NO_COLOR".into(), "1".into()),
                ("RUST_LOG".into(), "warn,cih_engine=info".into()),
            ],
            deadline: self.job_timeout,
            output_cap: self.output_cap,
        };
        self.spawn_job(job_id.clone(), canonical, spec).await;

        Ok(IndexSchedulerReceipt {
            job_id,
            deduplicated: false,
        })
    }

    async fn status(&self, job_id: &str) -> Result<IndexJobSnapshot, AppError> {
        self.jobs
            .read()
            .await
            .get(job_id)
            .cloned()
            .ok_or_else(|| AppError::NotFound {
                entity: "index job",
                key: job_id.to_string(),
            })
    }

    async fn cancel(&self, job_id: &str) -> Result<(), AppError> {
        self.signal_cancel(job_id).await
    }

    async fn metrics(&self) -> IndexQueueMetrics {
        let jobs = self.jobs.read().await;
        IndexQueueMetrics {
            queued: jobs
                .values()
                .filter(|state| matches!(state, JobState::Queued { .. }))
                .count(),
            running: jobs
                .values()
                .filter(|state| matches!(state, JobState::Running { .. }))
                .count(),
            rejected: self.rejected.load(Ordering::Relaxed),
        }
    }
}

/// Resolves when the job's cancel signal fires; pends forever if the sender
/// is dropped without cancelling (i.e. the job settles normally).
async fn wait_cancelled(mut rx: watch::Receiver<bool>) {
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use cih_core::{Registry, RegistryEntry};

    #[derive(Default)]
    struct FakeProcessRunner;

    #[async_trait]
    impl EngineProcessRunner for FakeProcessRunner {
        async fn run(
            &self,
            spec: EngineProcessSpec,
            mut cancel: watch::Receiver<bool>,
        ) -> EngineProcessOutcome {
            if spec.args.first().is_some_and(|arg| arg == "wait") {
                loop {
                    if *cancel.borrow() {
                        return EngineProcessOutcome::Cancelled;
                    }
                    if cancel.changed().await.is_err() {
                        return EngineProcessOutcome::LaunchFailed("cancel channel closed".into());
                    }
                }
            }
            EngineProcessOutcome::Exited {
                code: 0,
                success: true,
                stdout: "done".into(),
                stderr: String::new(),
                truncated: false,
            }
        }
    }

    fn entry(name: &str, path: &str, graph_key: &str) -> RegistryEntry {
        RegistryEntry {
            name: name.to_string(),
            path: path.to_string(),
            graph_key: graph_key.to_string(),
            artifacts_dir: String::new(),
            community_artifacts_dir: None,
            indexed_at: String::new(),
            last_git_head: None,
            stats: Default::default(),
        }
    }

    fn test_scheduler(max_concurrent: usize, queue_capacity: usize) -> IndexScheduler {
        IndexScheduler::with_runner_limits(
            std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            max_concurrent,
            queue_capacity,
            Duration::from_secs(60),
            64 * 1024,
            Arc::new(crate::infrastructure::artifact_repository::ArtifactCache::default()),
            Arc::new(FakeProcessRunner),
            IndexEngineConfig {
                backend: "memory".into(),
                falkor_url: String::new(),
            },
        )
    }

    fn command(languages: &str) -> IndexCommandKey {
        let mut languages: Vec<String> = languages
            .split(',')
            .map(str::trim)
            .filter(|language| !language.is_empty())
            .map(str::to_string)
            .collect();
        languages.sort();
        languages.dedup();
        IndexCommandKey::new("graph".into(), languages)
    }

    /// A registered path uses its own registry key — never the server primary.
    #[test]
    fn registered_path_uses_its_registry_key() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let reg = Registry {
            entries: vec![entry("svc", &canonical.display().to_string(), "svc-key")],
        };
        assert_eq!(
            resolve_target_graph_key(&reg, &canonical, "").unwrap(),
            "svc-key"
        );
        // An explicit key matching the registry entry is accepted too.
        assert_eq!(
            resolve_target_graph_key(&reg, &canonical, "svc-key").unwrap(),
            "svc-key"
        );
    }

    #[test]
    fn registered_path_rejects_conflicting_explicit_key() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let reg = Registry {
            entries: vec![entry("svc", &canonical.display().to_string(), "svc-key")],
        };
        let err = resolve_target_graph_key(&reg, &canonical, "primary").unwrap_err();
        assert!(
            err.contains("registered under graph key 'svc-key'"),
            "{err}"
        );
    }

    /// The S9 regression: an unregistered path must not silently land under
    /// any implicit key — the caller has to name a fresh one.
    #[test]
    fn unregistered_path_requires_explicit_key() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let reg = Registry {
            entries: vec![entry("other", "/somewhere/else", "primary")],
        };
        let err = resolve_target_graph_key(&reg, &canonical, "").unwrap_err();
        assert!(err.contains("pass an explicit `graph_key`"), "{err}");
        assert_eq!(
            resolve_target_graph_key(&reg, &canonical, "new-key").unwrap(),
            "new-key"
        );
    }

    #[test]
    fn unregistered_path_rejects_key_owned_by_another_repo() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let reg = Registry {
            entries: vec![entry("other", "/somewhere/else", "primary")],
        };
        let err = resolve_target_graph_key(&reg, &canonical, "primary").unwrap_err();
        assert!(err.contains("already owned by repo 'other'"), "{err}");
    }

    /// §13.2: one active job per repo — a duplicate submission coalesces onto
    /// the existing job instead of starting a second engine run.
    #[tokio::test]
    async fn duplicate_submissions_coalesce_onto_the_active_job() {
        let sched = test_scheduler(1, 4);
        let repo = Path::new("/tmp/repo-a");
        assert!(matches!(
            sched.admit(repo, "job-1", &command("rust,java")).await,
            Admission::Admitted
        ));
        match sched.admit(repo, "job-2", &command("java,rust")).await {
            Admission::Duplicate(existing) => assert_eq!(existing, "job-1"),
            other => panic!("expected Duplicate, got {}", admission_name(&other)),
        }
        // After the job completes (repo released), a new one is admitted.
        sched.active.lock().await.remove(repo);
        assert!(matches!(
            sched.admit(repo, "job-3", &command("rust")).await,
            Admission::Admitted
        ));
    }

    #[tokio::test]
    async fn different_command_for_active_repo_is_a_conflict() {
        let sched = test_scheduler(1, 4);
        let repo = Path::new("/tmp/repo-a");
        assert!(matches!(
            sched.admit(repo, "job-1", &command("rust")).await,
            Admission::Admitted
        ));
        assert!(matches!(
            sched.admit(repo, "job-2", &command("java")).await,
            Admission::Conflict { existing } if existing == "job-1"
        ));
    }

    /// §13.2: queued + running jobs are bounded; overflow is rejected.
    #[tokio::test]
    async fn admission_rejects_when_the_queue_is_full() {
        let sched = test_scheduler(1, 1); // capacity: 1 running + 1 queued
        assert!(matches!(
            sched
                .admit(Path::new("/tmp/a"), "job-a", &command(""))
                .await,
            Admission::Admitted
        ));
        assert!(matches!(
            sched
                .admit(Path::new("/tmp/b"), "job-b", &command(""))
                .await,
            Admission::Admitted
        ));
        match sched
            .admit(Path::new("/tmp/c"), "job-c", &command(""))
            .await
        {
            Admission::QueueFull { active, capacity } => {
                assert_eq!((active, capacity), (2, 2));
            }
            other => panic!("expected QueueFull, got {}", admission_name(&other)),
        }
    }

    fn admission_name(a: &Admission) -> &'static str {
        match a {
            Admission::Duplicate(_) => "Duplicate",
            Admission::Conflict { .. } => "Conflict",
            Admission::QueueFull { .. } => "QueueFull",
            Admission::Admitted => "Admitted",
        }
    }

    /// Poll until the job reaches a state matching `pred` (bounded wait).
    async fn wait_for(
        sched: &IndexScheduler,
        job_id: &str,
        pred: impl Fn(&JobState) -> bool,
    ) -> JobState {
        for _ in 0..200 {
            if let Some(state) = sched.jobs.read().await.get(job_id) {
                if pred(state) {
                    return state.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for job '{job_id}' state");
    }

    fn fake_spec(action: &str) -> EngineProcessSpec {
        EngineProcessSpec {
            program: PathBuf::from("fake-cih-engine"),
            args: vec![action.into()],
            current_dir: PathBuf::from("/tmp"),
            env: Vec::new(),
            deadline: Duration::from_secs(60),
            output_cap: 1024,
        }
    }

    /// §13: cancelling a running job kills the engine promptly, settles the
    /// job as `cancelled`, and releases the repo + cancel channel.
    #[tokio::test]
    async fn cancel_kills_a_running_job() {
        let sched = test_scheduler(1, 4);
        let repo = Path::new("/tmp/cancel-running");
        assert!(matches!(
            sched.admit(repo, "job-r", &command("")).await,
            Admission::Admitted
        ));
        let start = std::time::Instant::now();
        sched
            .spawn_job("job-r".into(), repo.to_path_buf(), fake_spec("wait"))
            .await;
        wait_for(&sched, "job-r", |s| matches!(s, JobState::Running { .. })).await;
        sched.signal_cancel("job-r").await.unwrap();
        wait_for(&sched, "job-r", |s| matches!(s, JobState::Cancelled { .. })).await;
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "cancel must not wait out the 30s sleep"
        );
        assert!(
            sched.active.lock().await.is_empty(),
            "repo must be released"
        );
        assert!(
            sched.cancels.lock().await.is_empty(),
            "cancel channel must be dropped"
        );
    }

    /// Cancelling while still queued settles the job without ever running it.
    #[tokio::test]
    async fn cancel_while_queued_settles_without_running() {
        // Zero running slots: the job can only ever be queued.
        let sched = test_scheduler(0, 4);
        let repo = Path::new("/tmp/cancel-queued");
        assert!(matches!(
            sched.admit(repo, "job-q", &command("")).await,
            Admission::Admitted
        ));
        sched
            .spawn_job("job-q".into(), repo.to_path_buf(), fake_spec("wait"))
            .await;
        wait_for(&sched, "job-q", |s| matches!(s, JobState::Queued { .. })).await;
        sched.signal_cancel("job-q").await.unwrap();
        wait_for(&sched, "job-q", |s| matches!(s, JobState::Cancelled { .. })).await;
        assert!(sched.active.lock().await.is_empty());
    }

    #[tokio::test]
    async fn cancel_unknown_or_finished_job_errors() {
        let sched = test_scheduler(1, 4);
        let err = sched.signal_cancel("nope").await.unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");

        // A finished job can't be cancelled — the error names its state.
        let repo = Path::new("/tmp/cancel-done");
        assert!(matches!(
            sched.admit(repo, "job-d", &command("")).await,
            Admission::Admitted
        ));
        sched
            .spawn_job("job-d".into(), repo.to_path_buf(), fake_spec("success"))
            .await;
        wait_for(&sched, "job-d", |s| matches!(s, JobState::Done { .. })).await;
        let err = sched.signal_cancel("job-d").await.unwrap_err();
        assert!(err.to_string().contains("already finished (done)"), "{err}");
    }

    #[tokio::test]
    async fn successful_job_invalidates_retained_repo_artifacts() {
        use crate::domain::repository::ResolvedRepo;
        use crate::infrastructure::artifact_repository::ArtifactCache;
        use crate::ports::artifact_repository::ArtifactRepository;

        let dir = tempfile::tempdir().unwrap();
        let artifacts_dir = dir.path().join(".cih").join("artifacts").join("v1");
        std::fs::create_dir_all(&artifacts_dir).unwrap();
        std::fs::write(artifacts_dir.join("nodes.jsonl"), "").unwrap();
        std::fs::write(artifacts_dir.join("edges.jsonl"), "").unwrap();
        let repo = ResolvedRepo::from_entry(cih_core::RegistryEntry {
            name: "fixture".into(),
            path: dir.path().display().to_string(),
            graph_key: "fixture".into(),
            artifacts_dir: artifacts_dir.display().to_string(),
            community_artifacts_dir: None,
            indexed_at: String::new(),
            last_git_head: None,
            stats: Default::default(),
        });
        let artifacts = Arc::new(ArtifactCache::default());
        artifacts.snapshot(&repo).await.unwrap();
        assert_eq!(artifacts.metrics().retained_entries, 1);

        let sched = IndexScheduler::with_runner_limits(
            Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            1,
            1,
            Duration::from_secs(5),
            1024,
            artifacts.clone(),
            Arc::new(FakeProcessRunner),
            IndexEngineConfig {
                backend: "memory".into(),
                falkor_url: String::new(),
            },
        );
        let canonical = dir.path().canonicalize().unwrap();
        assert!(matches!(
            sched
                .admit(&canonical, "job-invalidate", &command(""))
                .await,
            Admission::Admitted
        ));
        sched
            .spawn_job("job-invalidate".into(), canonical, fake_spec("success"))
            .await;
        wait_for(&sched, "job-invalidate", |state| {
            matches!(state, JobState::Done { .. })
        })
        .await;

        let metrics = artifacts.metrics();
        assert_eq!(metrics.invalidations, 1);
        assert_eq!(metrics.retained_entries, 0);
    }
}
