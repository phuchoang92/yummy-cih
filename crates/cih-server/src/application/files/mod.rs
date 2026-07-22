use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use serde::Serialize;
use tokio::sync::Semaphore;

use crate::application::app_services::RepoContextService;
use crate::domain::error::AppError;
use crate::domain::repository::RepoSelector;
use crate::ports::blocking_runtime::{blocking_timeout, run_blocking, BlockingError};
use crate::ports::retrieval_metrics::GrepRuntimeMetricsSnapshot;

#[derive(Clone)]
pub(crate) struct FileService {
    repos: RepoContextService,
    limits: ReadFileLimits,
}

impl FileService {
    pub(crate) fn new(repos: RepoContextService, limits: ReadFileLimits) -> Self {
        Self { repos, limits }
    }

    pub(crate) async fn read_file(
        &self,
        command: ReadFileCommand,
    ) -> Result<ReadFileOutput, AppError> {
        let repo = self
            .repos
            .resolve_repo(RepoSelector::from_wire(&command.repo))?;
        read_file(repo.canonical_path, self.limits, command).await
    }

    pub(crate) async fn grep_files(
        &self,
        command: GrepFilesCommand,
    ) -> Result<GrepFilesOutput, AppError> {
        let repo = self
            .repos
            .resolve_repo(RepoSelector::from_wire(&command.repo))?;
        grep_files(repo.canonical_path, command).await
    }
}

pub(crate) struct ReadFileCommand {
    pub(crate) repo: String,
    pub(crate) path: String,
    pub(crate) start_line: u32,
    pub(crate) end_line: u32,
}

pub(crate) struct GrepFilesCommand {
    pub(crate) repo: String,
    pub(crate) pattern: String,
    pub(crate) glob: String,
    pub(crate) limit: usize,
}

/// Caps applied by `read_file` to keep large files out of the agent's context.
#[derive(Clone, Copy)]
pub struct ReadFileLimits {
    /// Reject files larger than this many bytes.
    pub max_bytes: u64,
    /// Cap on returned lines when the caller gives no explicit range.
    pub max_lines: usize,
}

async fn read_file(
    repo_root: PathBuf,
    limits: ReadFileLimits,
    command: ReadFileCommand,
) -> Result<ReadFileOutput, AppError> {
    let clean = std::path::Path::new(&command.path);
    if clean
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(invalid("path", "must not contain '..' components"));
    }

    let path_label = command.path;
    let start_line = command.start_line;
    let end_line = command.end_line;
    let value = run_blocking(blocking_timeout(), "read file", move || {
        let full_path = repo_root.join(&path_label);

        // Resolve symlinks before the containment check so an in-repo symlink
        // cannot point outside the repository root.
        let canon_root = repo_root
            .canonicalize()
            .map_err(|error| invalid("path", format!("cannot resolve repo root: {error}")))?;
        let canon_path = full_path
            .canonicalize()
            .map_err(|error| invalid("path", format!("cannot resolve '{path_label}': {error}")))?;
        if !canon_path.starts_with(&canon_root) {
            return Err(invalid("path", "escapes repo root"));
        }

        read_sliced(&canon_path, &path_label, limits, start_line, end_line)
    })
    .await
    .map_err(blocking_error)??;
    Ok(value)
}

/// Size-check, read, and line-slice a resolved file path. Separated from repo
/// resolution so it is unit-testable without the registry.
fn read_sliced(
    full_path: &std::path::Path,
    path_label: &str,
    limits: ReadFileLimits,
    start_line: u32,
    end_line: u32,
) -> Result<ReadFileOutput, AppError> {
    // Reject oversized files before reading them into memory.
    let file_size = std::fs::metadata(full_path)
        .map_err(|error| invalid("path", format!("cannot stat '{path_label}': {error}")))?
        .len();
    if file_size > limits.max_bytes {
        return Err(invalid(
            "path",
            format!(
                "file '{path_label}' is {file_size} bytes, over the {}-byte read limit. \
                 Pass start_line/end_line to read a section, or raise CIH_READ_FILE_MAX_BYTES.",
                limits.max_bytes
            ),
        ));
    }

    let content = std::fs::read_to_string(full_path)
        .map_err(|error| invalid("path", format!("cannot read '{path_label}': {error}")))?;

    let explicit_range = start_line != 0 || end_line != 0;
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len() as u32;
    let start = (if start_line == 0 { 1 } else { start_line }).max(1);
    let mut end = (if end_line == 0 { total } else { end_line }).min(total);

    // With no explicit range, cap the number of returned lines so a very long
    // file doesn't flood the agent's context. Tell the caller when we truncate.
    let mut truncated = false;
    if !explicit_range && end >= start && (end - start + 1) as usize > limits.max_lines {
        end = start + limits.max_lines as u32 - 1;
        truncated = true;
    }

    let slice = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            let ln = *i as u32 + 1;
            ln >= start && ln <= end
        })
        .map(|(i, line)| format!("{:>4} {}", i as u32 + 1, line))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(ReadFileOutput {
        path: path_label.to_string(),
        total_lines: total,
        start_line: start,
        end_line: end.min(total),
        truncated,
        note: if truncated {
            Some(format!(
                "output capped at {} lines; pass start_line/end_line to read further",
                limits.max_lines
            ))
        } else {
            None
        },
        content: slice,
    })
}

/// Skip files larger than this during a grep walk — keeps stray artifacts
/// (fat jars, dumps) from being pulled into memory.
const GREP_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Cap on returned match text — one minified single-line file must not flood
/// the agent's context through a single match.
const GREP_MAX_TEXT_BYTES: usize = 500;
/// Aggregate response payload budget, including file names and match text.
const GREP_MAX_OUTPUT_BYTES: usize = 512 * 1024;
/// Default / hard-cap on the number of returned matches.
const GREP_DEFAULT_LIMIT: usize = 200;
const GREP_MAX_LIMIT: usize = 1000;

/// Build/vendor directories to skip even when no gitignore applies (sources
/// copied without `.git` — e.g. into a Docker volume — get no gitignore
/// filtering from the `ignore` crate).
const GREP_SKIP_DIRS: &[&str] = &["target", "node_modules", "build", "dist", ".git", ".cih"];

#[derive(Debug, Serialize)]
pub struct GrepMatch {
    pub file: String,
    pub line: u32,
    pub text: String,
}

#[derive(Clone, Copy, Debug)]
struct GrepConfig {
    max_concurrent_requests: usize,
    threads: usize,
    queue_timeout: Duration,
    deadline: Duration,
}

struct GrepRuntime {
    config: GrepConfig,
    lane: Arc<Semaphore>,
    pool: rayon::ThreadPool,
    metrics: GrepRuntimeMetrics,
}

#[derive(Default)]
struct GrepRuntimeMetrics {
    active: AtomicUsize,
    queued: AtomicUsize,
    rejected: AtomicU64,
    requests: AtomicU64,
    partial: AtomicU64,
    deadline_partial: AtomicU64,
    queue_wait_ms: AtomicU64,
    elapsed_ms: AtomicU64,
    candidate_files: AtomicU64,
    files_scanned: AtomicU64,
    files_skipped: AtomicU64,
    matches_returned: AtomicU64,
}

impl GrepRuntimeMetrics {
    fn snapshot(&self) -> GrepRuntimeMetricsSnapshot {
        GrepRuntimeMetricsSnapshot {
            active: self.active.load(Ordering::Relaxed),
            queued: self.queued.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            requests: self.requests.load(Ordering::Relaxed),
            partial: self.partial.load(Ordering::Relaxed),
            deadline_partial: self.deadline_partial.load(Ordering::Relaxed),
            queue_wait_ms: self.queue_wait_ms.load(Ordering::Relaxed),
            elapsed_ms: self.elapsed_ms.load(Ordering::Relaxed),
            candidate_files: self.candidate_files.load(Ordering::Relaxed),
            files_scanned: self.files_scanned.load(Ordering::Relaxed),
            files_skipped: self.files_skipped.load(Ordering::Relaxed),
            matches_returned: self.matches_returned.load(Ordering::Relaxed),
        }
    }
}

static GREP_RUNTIME: OnceLock<Result<GrepRuntime, String>> = OnceLock::new();

fn env_positive_usize(name: &str, default: usize) -> Result<usize, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| format!("{name} must be a positive integer")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(format!("cannot read {name}: {error}")),
    }
}

fn env_positive_u64(name: &str, default: u64) -> Result<u64, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| format!("{name} must be a positive integer")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(format!("cannot read {name}: {error}")),
    }
}

impl GrepRuntime {
    fn from_env() -> Result<Self, String> {
        let logical_cpus = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        let config = GrepConfig {
            max_concurrent_requests: env_positive_usize("CIH_GREP_MAX_CONCURRENT_REQUESTS", 1)?,
            threads: env_positive_usize("CIH_GREP_THREADS", logical_cpus.min(4))?,
            queue_timeout: Duration::from_secs(env_positive_u64("CIH_GREP_QUEUE_TIMEOUT_SECS", 2)?),
            deadline: Duration::from_secs(env_positive_u64("CIH_GREP_DEADLINE_SECS", 80)?),
        };
        let required = config
            .deadline
            .checked_add(Duration::from_secs(5))
            .ok_or_else(|| "CIH_GREP_DEADLINE_SECS is too large".to_string())?;
        if required > blocking_timeout() {
            return Err(format!(
                "CIH_GREP_DEADLINE_SECS ({}) must leave at least 5 seconds before CIH_BLOCKING_TIMEOUT_SECS ({})",
                config.deadline.as_secs(),
                blocking_timeout().as_secs()
            ));
        }
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.threads)
            .thread_name(|index| format!("cih-grep-{index}"))
            .build()
            .map_err(|error| format!("cannot create grep worker pool: {error}"))?;
        Ok(Self {
            config,
            lane: Arc::new(Semaphore::new(config.max_concurrent_requests)),
            pool,
            metrics: GrepRuntimeMetrics::default(),
        })
    }
}

fn grep_runtime() -> Result<&'static GrepRuntime, AppError> {
    match GREP_RUNTIME.get_or_init(GrepRuntime::from_env) {
        Ok(runtime) => Ok(runtime),
        Err(message) => Err(AppError::Unavailable {
            dependency: "grep configuration",
            message: message.clone(),
            retryable: false,
        }),
    }
}

pub(crate) fn validate_grep_runtime() -> Result<(), AppError> {
    grep_runtime().map(|_| ())
}

pub(crate) fn grep_runtime_metrics() -> GrepRuntimeMetricsSnapshot {
    GREP_RUNTIME
        .get()
        .and_then(|runtime| runtime.as_ref().ok())
        .map(|runtime| runtime.metrics.snapshot())
        .unwrap_or_default()
}

struct GaugeGuard<'a>(&'a AtomicUsize);

impl<'a> GaugeGuard<'a> {
    fn enter(value: &'a AtomicUsize) -> Self {
        value.fetch_add(1, Ordering::Relaxed);
        Self(value)
    }
}

impl Drop for GaugeGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

struct CancellationGuard {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

impl CancellationGuard {
    fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self {
            cancelled,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

async fn grep_files(
    repo_root: PathBuf,
    command: GrepFilesCommand,
) -> Result<GrepFilesOutput, AppError> {
    let regex = compile_pattern(&command.pattern)?;
    let overrides = compile_glob_override(&repo_root, &command.glob)?;

    let limit = if command.limit == 0 {
        GREP_DEFAULT_LIMIT
    } else {
        command.limit
    }
    .min(GREP_MAX_LIMIT);

    let runtime = grep_runtime()?;
    let queued_at = Instant::now();
    let queued = GaugeGuard::enter(&runtime.metrics.queued);
    let permit = match tokio::time::timeout(
        runtime.config.queue_timeout,
        runtime.lane.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => permit,
        Ok(Err(error)) => {
            return Err(AppError::Unavailable {
                dependency: "grep",
                message: format!("grep admission closed: {error}"),
                retryable: true,
            })
        }
        Err(_) => {
            runtime.metrics.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(AppError::Unavailable {
                dependency: "grep",
                message: format!(
                    "grep capacity saturated after {}s; retry shortly",
                    runtime.config.queue_timeout.as_secs()
                ),
                retryable: true,
            });
        }
    };
    drop(queued);
    runtime.metrics.queue_wait_ms.fetch_add(
        queued_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        Ordering::Relaxed,
    );
    runtime.metrics.requests.fetch_add(1, Ordering::Relaxed);

    let started = Instant::now();
    let deadline = started + runtime.config.deadline;
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut cancellation = CancellationGuard::new(cancelled.clone());
    let scan = run_blocking(blocking_timeout(), "grep", move || {
        // The permit deliberately lives in the closure. If the async caller
        // disconnects or the outer timeout fires, no second repository scan
        // can start until this cooperative scan has actually exited.
        let _permit = permit;
        let _active = GaugeGuard::enter(&runtime.metrics.active);
        grep_dir(
            &repo_root,
            &regex,
            GrepScanOptions {
                overrides,
                limit,
                started,
                deadline,
                cancelled: &cancelled,
                pool: &runtime.pool,
                threads: runtime.config.threads,
            },
        )
    })
    .await;
    let scan = match scan {
        Ok(scan) => {
            cancellation.disarm();
            scan
        }
        Err(error) => return Err(grep_blocking_error(error)),
    };
    if !scan.complete {
        runtime.metrics.partial.fetch_add(1, Ordering::Relaxed);
        if scan.truncation_reason == GrepTruncationReason::Deadline {
            runtime
                .metrics
                .deadline_partial
                .fetch_add(1, Ordering::Relaxed);
        }
        tracing::info!(
            reason = ?scan.truncation_reason,
            candidate_files = scan.candidate_files,
            files_scanned = scan.files_scanned,
            files_skipped = scan.files_skipped,
            elapsed_ms = scan.elapsed_ms,
            "grep returned a partial result"
        );
    }
    runtime
        .metrics
        .elapsed_ms
        .fetch_add(scan.elapsed_ms, Ordering::Relaxed);
    runtime.metrics.candidate_files.fetch_add(
        u64::try_from(scan.candidate_files).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    runtime.metrics.files_scanned.fetch_add(
        u64::try_from(scan.files_scanned).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    runtime.metrics.files_skipped.fetch_add(
        u64::try_from(scan.files_skipped).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    runtime.metrics.matches_returned.fetch_add(
        u64::try_from(scan.matches.len()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );

    Ok(GrepFilesOutput {
        pattern: command.pattern,
        glob: command.glob,
        matches_returned: scan.matches.len(),
        truncated: !scan.complete,
        complete: scan.complete,
        truncation_reason: scan.truncation_reason,
        candidate_files: scan.candidate_files,
        files_scanned: scan.files_scanned,
        files_skipped: scan.files_skipped,
        elapsed_ms: scan.elapsed_ms,
        matches: scan.matches,
    })
}

fn compile_pattern(pattern: &str) -> Result<regex::Regex, AppError> {
    regex::Regex::new(pattern)
        .map_err(|error| invalid("pattern", format!("invalid regex pattern: {error}")))
}

fn compile_glob_override(
    root: &Path,
    glob: &str,
) -> Result<Option<ignore::overrides::Override>, AppError> {
    if glob.is_empty() {
        return Ok(None);
    }
    let mut builder = ignore::overrides::OverrideBuilder::new(root);
    builder
        .add(glob)
        .map_err(|error| invalid("glob", format!("invalid glob: {error}")))?;
    builder
        .build()
        .map_err(|error| invalid("glob", format!("invalid glob: {error}")))
        .map(Some)
}

#[derive(Debug, Serialize)]
pub(crate) struct ReadFileOutput {
    pub(crate) path: String,
    pub(crate) total_lines: u32,
    pub(crate) start_line: u32,
    pub(crate) end_line: u32,
    pub(crate) truncated: bool,
    pub(crate) note: Option<String>,
    pub(crate) content: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct GrepFilesOutput {
    pub(crate) pattern: String,
    pub(crate) glob: String,
    pub(crate) matches_returned: usize,
    pub(crate) truncated: bool,
    pub(crate) complete: bool,
    pub(crate) truncation_reason: GrepTruncationReason,
    pub(crate) candidate_files: usize,
    pub(crate) files_scanned: usize,
    pub(crate) files_skipped: usize,
    pub(crate) elapsed_ms: u64,
    pub(crate) matches: Vec<GrepMatch>,
}

fn invalid(field: &'static str, message: impl Into<String>) -> AppError {
    AppError::InvalidInput {
        field,
        message: message.into(),
    }
}

fn blocking_error(error: crate::ports::blocking_runtime::BlockingError) -> AppError {
    AppError::Unavailable {
        dependency: "blocking runtime",
        message: error.to_string(),
        retryable: true,
    }
}

fn grep_blocking_error(error: BlockingError) -> AppError {
    let message = match error {
        BlockingError::TimedOut { secs, .. } => format!("grep timed out after {secs}s"),
        BlockingError::Panicked { detail, .. } => format!("grep task panicked: {detail}"),
        BlockingError::Saturated { waited_secs, .. } => {
            format!("grep capacity saturated after {waited_secs}s")
        }
    };
    AppError::Unavailable {
        dependency: "grep",
        message,
        retryable: true,
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GrepTruncationReason {
    None,
    Limit,
    Bytes,
    Deadline,
    Cancelled,
}

#[derive(Debug)]
struct GrepScan {
    matches: Vec<GrepMatch>,
    complete: bool,
    truncation_reason: GrepTruncationReason,
    candidate_files: usize,
    files_scanned: usize,
    files_skipped: usize,
    elapsed_ms: u64,
}

struct CandidateFile {
    path: PathBuf,
    relative: PathBuf,
}

struct FileScan {
    matches: Vec<GrepMatch>,
    scanned: bool,
    skipped: bool,
    stopped: Option<GrepTruncationReason>,
}

struct GrepScanOptions<'a> {
    overrides: Option<ignore::overrides::Override>,
    limit: usize,
    started: Instant,
    deadline: Instant,
    cancelled: &'a AtomicBool,
    pool: &'a rayon::ThreadPool,
    threads: usize,
}

fn cooperative_stop(cancelled: &AtomicBool, deadline: Instant) -> Option<GrepTruncationReason> {
    if cancelled.load(Ordering::Acquire) {
        Some(GrepTruncationReason::Cancelled)
    } else if Instant::now() >= deadline {
        Some(GrepTruncationReason::Deadline)
    } else {
        None
    }
}

/// Gitignore-aware, glob-pruned regex scan under `root`. Candidate paths are
/// sorted before small parallel batches are processed, keeping result order
/// deterministic while bounding disk concurrency and transient line buffers.
fn grep_dir(root: &Path, regex: &regex::Regex, options: GrepScanOptions<'_>) -> GrepScan {
    let GrepScanOptions {
        overrides,
        limit,
        started,
        deadline,
        cancelled,
        pool,
        threads,
    } = options;
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .add_custom_ignore_filename(".cihignore")
        .filter_entry(|entry| {
            if entry.depth() > 0 && entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                let name = entry.file_name().to_string_lossy();
                return !GREP_SKIP_DIRS.contains(&name.as_ref());
            }
            true
        });
    if let Some(overrides) = overrides {
        builder.overrides(overrides);
    }

    let mut candidates = Vec::new();
    let mut candidate_files = 0usize;
    let mut files_skipped = 0usize;
    let mut traversal_stop = None;
    for entry in builder.build() {
        if let Some(reason) = cooperative_stop(cancelled, deadline) {
            traversal_stop = Some(reason);
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                files_skipped = files_skipped.saturating_add(1);
                continue;
            }
        };
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        if entry.path_is_symlink() {
            files_skipped = files_skipped.saturating_add(1);
            continue;
        }
        let rel = match entry.path().strip_prefix(root) {
            Ok(rel) => rel,
            Err(_) => {
                files_skipped = files_skipped.saturating_add(1);
                continue;
            }
        };
        candidate_files = candidate_files.saturating_add(1);
        match entry.metadata() {
            Ok(md) if md.len() <= GREP_MAX_FILE_BYTES => {}
            _ => {
                files_skipped = files_skipped.saturating_add(1);
                continue;
            }
        }
        let relative = rel.to_path_buf();
        candidates.push(CandidateFile {
            path: entry.into_path(),
            relative,
        });
    }
    candidates.sort_by(|left, right| left.relative.cmp(&right.relative));

    let mut matches = Vec::new();
    let mut files_scanned = 0usize;
    let mut output_bytes = 0usize;
    let mut reason = traversal_stop.unwrap_or(GrepTruncationReason::None);
    if reason == GrepTruncationReason::None {
        'batches: for batch in candidates.chunks(threads.max(1)) {
            if let Some(stopped) = cooperative_stop(cancelled, deadline) {
                reason = stopped;
                break;
            }
            let scans: Vec<FileScan> = pool.install(|| {
                batch
                    .par_iter()
                    .map(|candidate| scan_candidate(candidate, regex, limit, deadline, cancelled))
                    .collect()
            });
            for scan in &scans {
                files_scanned = files_scanned.saturating_add(usize::from(scan.scanned));
                files_skipped = files_skipped.saturating_add(usize::from(scan.skipped));
                if reason == GrepTruncationReason::None {
                    if let Some(stopped) = scan.stopped {
                        reason = stopped;
                    }
                }
            }
            for scan in scans {
                for found in scan.matches {
                    let match_bytes = found
                        .file
                        .len()
                        .saturating_add(found.text.len())
                        .saturating_add(16);
                    if output_bytes.saturating_add(match_bytes) > GREP_MAX_OUTPUT_BYTES {
                        reason = GrepTruncationReason::Bytes;
                        break 'batches;
                    }
                    output_bytes = output_bytes.saturating_add(match_bytes);
                    matches.push(found);
                    if matches.len() >= limit {
                        reason = GrepTruncationReason::Limit;
                        break 'batches;
                    }
                }
            }
            if reason != GrepTruncationReason::None {
                break;
            }
        }
    }
    let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    GrepScan {
        matches,
        complete: reason == GrepTruncationReason::None,
        truncation_reason: reason,
        candidate_files,
        files_scanned,
        files_skipped,
        elapsed_ms,
    }
}

fn scan_candidate(
    candidate: &CandidateFile,
    regex: &regex::Regex,
    match_cap: usize,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> FileScan {
    let file = match std::fs::File::open(&candidate.path) {
        Ok(file) => file,
        Err(_) => {
            return FileScan {
                matches: Vec::new(),
                scanned: false,
                skipped: true,
                stopped: None,
            }
        }
    };
    let mut reader = std::io::BufReader::new(file);
    let mut bytes = Vec::new();
    let mut matches = Vec::new();
    let mut line_number = 0u32;
    loop {
        if let Some(reason) = cooperative_stop(cancelled, deadline) {
            return FileScan {
                matches,
                scanned: true,
                skipped: false,
                stopped: Some(reason),
            };
        }
        bytes.clear();
        let read = match reader.read_until(b'\n', &mut bytes) {
            Ok(read) => read,
            Err(_) => {
                return FileScan {
                    matches: Vec::new(),
                    scanned: false,
                    skipped: true,
                    stopped: None,
                }
            }
        };
        if read == 0 {
            break;
        }
        if bytes.contains(&0) {
            return FileScan {
                matches: Vec::new(),
                scanned: false,
                skipped: true,
                stopped: None,
            };
        }
        line_number = line_number.saturating_add(1);
        while matches!(bytes.last(), Some(b'\n' | b'\r')) {
            bytes.pop();
        }
        let line = String::from_utf8_lossy(&bytes);
        if regex.is_match(&line) {
            matches.push(GrepMatch {
                file: candidate.relative.to_string_lossy().into_owned(),
                line: line_number,
                text: cap_text(&line, GREP_MAX_TEXT_BYTES),
            });
            if matches.len() >= match_cap {
                break;
            }
        }
    }
    FileScan {
        matches,
        scanned: true,
        skipped: false,
        stopped: None,
    }
}

/// Truncate to at most `max` bytes on a char boundary, marking the cut.
fn cap_text(line: &str, max: usize) -> String {
    if line.len() <= max {
        return line.to_string();
    }
    let mut end = max;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_write(name: &str, contents: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("cih-readfile-test-{name}"));
        std::fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn oversized_file_is_rejected() {
        let p = tmp_write("big", &"x".repeat(1000));
        let limits = ReadFileLimits {
            max_bytes: 100,
            max_lines: 5000,
        };
        let err = read_sliced(&p, "big.txt", limits, 0, 0).unwrap_err();
        assert!(err.to_string().contains("over the"), "unexpected: {err}");
    }

    #[test]
    fn unranged_read_truncates_at_line_cap() {
        let body: String = (1..=20).map(|i| format!("line{i}\n")).collect();
        let p = tmp_write("lines", &body);
        let limits = ReadFileLimits {
            max_bytes: 10 * 1024 * 1024,
            max_lines: 5,
        };
        let v = serde_json::to_value(read_sliced(&p, "lines.txt", limits, 0, 0).unwrap()).unwrap();
        assert_eq!(v["truncated"], serde_json::json!(true));
        assert_eq!(v["total_lines"], serde_json::json!(20));
        assert_eq!(v["end_line"], serde_json::json!(5));
        assert!(v["content"].as_str().unwrap().contains("line5"));
        assert!(!v["content"].as_str().unwrap().contains("line6"));
    }

    #[test]
    fn explicit_range_is_not_capped() {
        let body: String = (1..=20).map(|i| format!("line{i}\n")).collect();
        let p = tmp_write("range", &body);
        let limits = ReadFileLimits {
            max_bytes: 10 * 1024 * 1024,
            max_lines: 5,
        };
        let v = serde_json::to_value(read_sliced(&p, "range.txt", limits, 1, 20).unwrap()).unwrap();
        assert_eq!(v["truncated"], serde_json::json!(false));
        assert_eq!(v["end_line"], serde_json::json!(20));
    }

    #[test]
    fn small_file_reads_whole() {
        let p = tmp_write("small", "a\nb\nc\n");
        let limits = ReadFileLimits {
            max_bytes: 10 * 1024 * 1024,
            max_lines: 5000,
        };
        let v = serde_json::to_value(read_sliced(&p, "small.txt", limits, 0, 0).unwrap()).unwrap();
        assert_eq!(v["truncated"], serde_json::json!(false));
        assert_eq!(v["total_lines"], serde_json::json!(3));
    }

    /// Fresh temp dir for a grep test; recreated on every run.
    fn grep_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("cih-grepfiles-test-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_under(root: &std::path::Path, rel: &str, contents: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    }

    fn re(pattern: &str) -> regex::Regex {
        regex::Regex::new(pattern).unwrap()
    }

    fn test_grep(root: &Path, pattern: &str, glob: &str, limit: usize) -> GrepScan {
        let regex = re(pattern);
        let overrides = compile_glob_override(root, glob).unwrap();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        let started = Instant::now();
        let cancelled = AtomicBool::new(false);
        grep_dir(
            root,
            &regex,
            GrepScanOptions {
                overrides,
                limit,
                started,
                deadline: started + Duration::from_secs(5),
                cancelled: &cancelled,
                pool: &pool,
                threads: 2,
            },
        )
    }

    #[test]
    fn grep_finds_match_with_file_line_text() {
        let root = grep_root("basic");
        write_under(
            &root,
            "src/Foo.java",
            b"class Foo {\n  // TODO fix this\n}\n",
        );
        let scan = test_grep(&root, "TODO", "", 100);
        assert!(scan.complete);
        assert_eq!(scan.truncation_reason, GrepTruncationReason::None);
        assert_eq!(scan.candidate_files, 1);
        assert_eq!(scan.files_scanned, 1);
        assert_eq!(scan.matches.len(), 1);
        assert_eq!(scan.matches[0].file, "src/Foo.java");
        assert_eq!(scan.matches[0].line, 2);
        assert_eq!(scan.matches[0].text, "  // TODO fix this");
    }

    #[test]
    fn grep_glob_filters_files() {
        let root = grep_root("glob");
        write_under(&root, "a/Foo.java", b"// TODO java\n");
        write_under(&root, "b/bar.rs", b"// TODO rust\n");
        let scan = test_grep(&root, "TODO", "**/*.java", 100);
        assert_eq!(scan.candidate_files, 1, "glob must prune during traversal");
        assert_eq!(scan.matches.len(), 1);
        assert_eq!(scan.matches[0].file, "a/Foo.java");
    }

    #[test]
    fn grep_limit_truncates() {
        let root = grep_root("limit");
        let body: String = (1..=10).map(|i| format!("TODO {i}\n")).collect();
        write_under(&root, "many.txt", body.as_bytes());
        let scan = test_grep(&root, "TODO", "", 3);
        assert!(!scan.complete);
        assert_eq!(scan.truncation_reason, GrepTruncationReason::Limit);
        assert_eq!(scan.matches.len(), 3);
    }

    #[test]
    fn grep_skips_binary_files() {
        let root = grep_root("binary");
        write_under(&root, "blob.bin", b"TODO\0TODO\n");
        let scan = test_grep(&root, "TODO", "", 100);
        assert!(scan.matches.is_empty());
        assert_eq!(scan.files_scanned, 0);
        assert_eq!(scan.files_skipped, 1);
    }

    #[test]
    fn grep_caps_long_match_text() {
        let root = grep_root("longline");
        let line = format!("TODO {}", "x".repeat(2000));
        write_under(&root, "minified.js", line.as_bytes());
        let scan = test_grep(&root, "TODO", "", 100);
        assert_eq!(scan.matches.len(), 1);
        assert!(scan.matches[0].text.len() <= GREP_MAX_TEXT_BYTES + '…'.len_utf8());
        assert!(scan.matches[0].text.ends_with('…'));
    }

    #[test]
    fn grep_skips_build_dirs() {
        let root = grep_root("skipdirs");
        write_under(&root, "node_modules/dep/x.js", b"// TODO vendored\n");
        write_under(&root, "target/debug/x.rs", b"// TODO generated\n");
        write_under(&root, ".cih/artifacts/x.jsonl", b"// TODO artifact\n");
        write_under(&root, "src/x.rs", b"// TODO real\n");
        let scan = test_grep(&root, "TODO", "", 100);
        assert_eq!(scan.matches.len(), 1);
        assert_eq!(scan.matches[0].file, "src/x.rs");
    }

    #[test]
    fn grep_orders_parallel_results_by_relative_path() {
        let root = grep_root("ordered");
        write_under(&root, "z/last.rs", b"TODO last\n");
        write_under(&root, "a/first.rs", b"TODO first\n");
        let scan = test_grep(&root, "TODO", "", 100);
        let files: Vec<&str> = scan
            .matches
            .iter()
            .map(|found| found.file.as_str())
            .collect();
        assert_eq!(files, vec!["a/first.rs", "z/last.rs"]);
    }

    #[test]
    fn grep_byte_cap_returns_partial_metadata() {
        let root = grep_root("byte-cap");
        let body: String = (0..1000)
            .map(|_| format!("TODO {}\n", "x".repeat(GREP_MAX_TEXT_BYTES)))
            .collect();
        write_under(&root, "large.txt", body.as_bytes());
        let scan = test_grep(&root, "TODO", "", GREP_MAX_LIMIT);
        assert!(!scan.complete);
        assert_eq!(scan.truncation_reason, GrepTruncationReason::Bytes);
        assert!(scan.matches.len() < GREP_MAX_LIMIT);
    }

    #[test]
    fn grep_expired_deadline_returns_normal_partial_result() {
        let root = grep_root("deadline");
        write_under(&root, "src/x.rs", b"TODO real\n");
        let regex = re("TODO");
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let started = Instant::now();
        let cancelled = AtomicBool::new(false);
        let scan = grep_dir(
            &root,
            &regex,
            GrepScanOptions {
                overrides: None,
                limit: 100,
                started,
                deadline: started,
                cancelled: &cancelled,
                pool: &pool,
                threads: 1,
            },
        );
        assert!(!scan.complete);
        assert_eq!(scan.truncation_reason, GrepTruncationReason::Deadline);
    }

    #[test]
    fn invalid_pattern_is_rejected() {
        let err = compile_pattern("[unclosed").unwrap_err();
        assert!(
            err.to_string().contains("invalid regex"),
            "unexpected: {err}"
        );
    }
}
