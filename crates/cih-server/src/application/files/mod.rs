use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
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
    grep: Arc<GrepRuntime>,
}

impl FileService {
    pub(crate) fn new(
        repos: RepoContextService,
        limits: ReadFileLimits,
        grep: Arc<GrepRuntime>,
    ) -> Self {
        Self {
            repos,
            limits,
            grep,
        }
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
        grep_files(repo.canonical_path, command, self.grep.clone()).await
    }

    /// Raw (un-numbered) bounded line span for an already-resolved repository
    /// context — doc_pack's source section. Applies the same containment and
    /// whole-file size guard as `read_file`, then streams lines, cutting at
    /// `max_lines` / `max_bytes` on a UTF-8 character boundary.
    pub(crate) async fn read_span_in_context(
        &self,
        context: &crate::ports::repo_context_provider::RepoContext,
        command: SourceSpanCommand,
    ) -> Result<SourceSpan, AppError> {
        let repo_root = context.repo.canonical_path.clone();
        let max_file_bytes = self.limits.max_bytes;
        if std::path::Path::new(&command.path)
            .components()
            .any(|c| c == std::path::Component::ParentDir)
        {
            return Err(invalid("path", "must not contain '..' components"));
        }
        run_blocking(blocking_timeout(), "read source span", move || {
            let full_path = repo_root.join(&command.path);
            let canon_path = canonical_contained_target(&repo_root, &full_path).map_err(
                |error| match error {
                    ContainmentError::Root(e) => {
                        invalid("path", format!("cannot resolve repo root: {e}"))
                    }
                    ContainmentError::Target(e) => {
                        invalid("path", format!("cannot resolve '{}': {e}", command.path))
                    }
                    ContainmentError::Outside => invalid("path", "escapes repo root"),
                },
            )?;
            let file_size = std::fs::metadata(&canon_path)
                .map_err(|e| invalid("path", format!("cannot stat '{}': {e}", command.path)))?
                .len();
            if file_size > max_file_bytes {
                return Err(invalid(
                    "path",
                    format!(
                        "file '{}' is {file_size} bytes, over the {max_file_bytes}-byte read limit",
                        command.path
                    ),
                ));
            }
            read_span(&canon_path, command)
        })
        .await
        .map_err(blocking_error)?
    }
}

pub(crate) struct SourceSpanCommand {
    /// Repo-relative file path.
    pub(crate) path: String,
    /// 1-based inclusive; 0 means line 1.
    pub(crate) start_line: u32,
    /// 1-based inclusive; 0 means "until a cap or EOF".
    pub(crate) end_line: u32,
    pub(crate) max_lines: usize,
    pub(crate) max_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SourceSpan {
    pub(crate) path: String,
    pub(crate) start_line: u32,
    /// Last line that contributed content (may be partially included).
    pub(crate) end_line: u32,
    /// True when the requested span was cut by `max_lines`/`max_bytes`.
    pub(crate) truncated: bool,
    pub(crate) content: String,
}

/// Largest prefix of `text` that fits `budget` bytes without splitting a
/// UTF-8 character.
fn char_boundary_prefix(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    let mut cut = budget;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

fn read_span(
    full_path: &std::path::Path,
    command: SourceSpanCommand,
) -> Result<SourceSpan, AppError> {
    use std::io::BufRead;

    let file = std::fs::File::open(full_path)
        .map_err(|e| invalid("path", format!("cannot read '{}': {e}", command.path)))?;
    let reader = std::io::BufReader::new(file);
    let start = command.start_line.max(1);
    let requested_end = if command.end_line == 0 {
        u32::MAX
    } else {
        command.end_line.max(start)
    };
    let mut content = String::new();
    let mut end_line = start;
    let mut included = 0usize;
    let mut truncated = false;
    for (index, line) in reader.lines().enumerate() {
        let line =
            line.map_err(|e| invalid("path", format!("cannot read '{}': {e}", command.path)))?;
        let number = index as u32 + 1;
        if number < start {
            continue;
        }
        if number > requested_end {
            break;
        }
        if included == command.max_lines {
            truncated = true;
            break;
        }
        let separator = usize::from(included > 0);
        let remaining = command.max_bytes.saturating_sub(content.len());
        if separator > remaining {
            truncated = true;
            break;
        }
        let budget = remaining - separator;
        if line.len() > budget {
            let prefix = char_boundary_prefix(&line, budget);
            truncated = true;
            if !prefix.is_empty() {
                if included > 0 {
                    content.push('\n');
                }
                content.push_str(prefix);
                end_line = number;
            }
            break;
        }
        if included > 0 {
            content.push('\n');
        }
        content.push_str(&line);
        end_line = number;
        included += 1;
    }
    Ok(SourceSpan {
        path: command.path,
        start_line: start,
        end_line,
        truncated,
        content,
    })
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

/// Why [`canonical_contained_target`] rejected a candidate. Callers map each
/// variant to their own field-specific `AppError` wording (read_file's `path`,
/// the glob fast path's `glob`, doc_status's `docs_dir`).
#[derive(Debug)]
pub(crate) enum ContainmentError {
    /// The repository root itself could not be canonicalized.
    Root(std::io::Error),
    /// The candidate path could not be canonicalized (missing, permission, …).
    Target(std::io::Error),
    /// The resolved candidate escapes the resolved repository root.
    Outside,
}

/// The one canonical root/target containment check: resolve symlinks on both
/// sides, then require the resolved target to stay under the resolved root.
/// Every path-crossing feature (read_file, the literal-glob fast path,
/// doc_status's docs walk) must go through this instead of growing another
/// inline `canonicalize` + `starts_with` copy.
pub(crate) fn canonical_contained_target(
    root: &Path,
    candidate: &Path,
) -> Result<PathBuf, ContainmentError> {
    let canonical_root = root.canonicalize().map_err(ContainmentError::Root)?;
    let target = candidate.canonicalize().map_err(ContainmentError::Target)?;
    if !target.starts_with(&canonical_root) {
        return Err(ContainmentError::Outside);
    }
    Ok(target)
}

/// Stable repo-relative path spelling for serialized output and diagnostics.
/// Disk access stays on native [`Path`] values; only the wire label uses `/`
/// so callers and glob syntax see the same form on every operating system.
pub(crate) fn portable_relative_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
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
        let canon_path =
            canonical_contained_target(&repo_root, &full_path).map_err(|error| match error {
                ContainmentError::Root(e) => {
                    invalid("path", format!("cannot resolve repo root: {e}"))
                }
                ContainmentError::Target(e) => {
                    invalid("path", format!("cannot resolve '{path_label}': {e}"))
                }
                ContainmentError::Outside => invalid("path", "escapes repo root"),
            })?;

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
pub(crate) struct GrepRuntimeConfig {
    pub(crate) max_concurrent_requests: usize,
    pub(crate) threads: usize,
    pub(crate) queue_timeout: Duration,
    pub(crate) deadline: Duration,
}

pub(crate) struct GrepRuntime {
    config: GrepRuntimeConfig,
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

impl GrepRuntime {
    pub(crate) fn new(config: GrepRuntimeConfig) -> Result<Self, String> {
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

    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self::new(GrepRuntimeConfig {
            max_concurrent_requests: 2,
            threads: 2,
            queue_timeout: Duration::from_secs(2),
            deadline: Duration::from_secs(5),
        })
        .expect("test grep runtime")
    }

    pub(crate) fn metrics(&self) -> GrepRuntimeMetricsSnapshot {
        self.metrics.snapshot()
    }
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
    runtime: Arc<GrepRuntime>,
) -> Result<GrepFilesOutput, AppError> {
    let regex = compile_pattern(&command.pattern)?;
    let overrides = compile_glob_override(&repo_root, &command.glob)?;

    let limit = if command.limit == 0 {
        GREP_DEFAULT_LIMIT
    } else {
        command.limit
    }
    .min(GREP_MAX_LIMIT);

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
                    "grep capacity saturated after {}s; retry shortly or tune \
                     CIH_GREP_MAX_CONCURRENT_REQUESTS / CIH_GREP_QUEUE_TIMEOUT_SECS",
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
    let glob = command.glob.clone();
    let scan_runtime = runtime.clone();
    let scan = run_blocking(blocking_timeout(), "grep", move || {
        // The permit deliberately lives in the closure. If the async caller
        // disconnects or the outer timeout fires, no second repository scan
        // can start until this cooperative scan has actually exited.
        let _permit = permit;
        let _active = GaugeGuard::enter(&scan_runtime.metrics.active);
        grep_dir(
            &repo_root,
            &regex,
            GrepScanOptions {
                overrides,
                glob: &glob,
                limit,
                started,
                deadline,
                cancelled: &cancelled,
                pool: &scan_runtime.pool,
                threads: scan_runtime.config.threads,
            },
        )
    })
    .await;
    let scan = match scan {
        Ok(scan) => {
            cancellation.disarm();
            scan?
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
        dependency: "file read",
        message: error.to_string(),
        retryable: true,
    }
}

fn grep_blocking_error(error: BlockingError) -> AppError {
    let message = match error {
        BlockingError::TimedOut { secs, .. } => format!(
            "grep timed out after {secs}s; tune CIH_GREP_DEADLINE_SECS and keep \
             CIH_BLOCKING_TIMEOUT_SECS at least CIH_GREP_QUEUE_TIMEOUT_SECS + \
             CIH_GREP_DEADLINE_SECS + 5"
        ),
        BlockingError::Panicked { detail, .. } => format!("grep task panicked: {detail}"),
        BlockingError::Saturated { waited_secs, .. } => {
            format!(
                "grep blocking lane saturated after waiting {waited_secs}s \
                 (CIH_BLOCKING_MAX_CONCURRENT / CIH_BLOCKING_QUEUE_TIMEOUT_SECS)"
            )
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
    /// The raw glob (already compiled into `overrides`) — its literal prefix
    /// prunes the walk.
    glob: &'a str,
    limit: usize,
    started: Instant,
    deadline: Instant,
    cancelled: &'a AtomicBool,
    pool: &'a rayon::ThreadPool,
    threads: usize,
}

/// How much of the repo the walk must visit for a given glob.
#[derive(Debug, PartialEq, Eq)]
enum WalkPlan {
    /// No usable literal prefix — walk the whole repo root.
    Full,
    /// The glob's literal prefix is a directory — walk only that subtree.
    Subtree(PathBuf),
    /// The whole glob is a literal path to one file — scan just its contained,
    /// resolved target while reporting the original repo-relative path.
    SingleFile { path: PathBuf, relative: PathBuf },
    /// The literal prefix does not exist — nothing can match.
    Empty,
}

/// Deepest metacharacter-free, `..`-free path prefix of a glob, and whether the
/// prefix is the entire glob. `None` when the very first segment is a pattern.
fn literal_walk_prefix(glob: &str) -> Option<(PathBuf, bool)> {
    fn is_literal(segment: &str) -> bool {
        !segment.is_empty()
            && segment != ".."
            && segment != "."
            && !segment
                .chars()
                .any(|c| matches!(c, '*' | '?' | '[' | ']' | '{' | '}' | '!' | '\\'))
    }
    let glob = glob.strip_prefix("./").unwrap_or(glob);
    if glob.starts_with('/') {
        return None;
    }
    let segments: Vec<&str> = glob.split('/').collect();
    let literal_count = segments.iter().take_while(|s| is_literal(s)).count();
    if literal_count == 0 {
        return None;
    }
    let prefix: PathBuf = segments[..literal_count].iter().collect();
    Some((prefix, literal_count == segments.len()))
}

/// Decide the walk scope from the glob's literal prefix. On a 500k-file volume
/// this is the difference between one `stat` and a full-tree traversal for a
/// single-file grep.
fn plan_walk(root: &Path, glob: &str) -> Result<WalkPlan, AppError> {
    if glob.is_empty() {
        return Ok(WalkPlan::Full);
    }
    let Some((prefix, whole_glob)) = literal_walk_prefix(glob) else {
        return Ok(WalkPlan::Full);
    };

    // Inspect each component without following links. A symlink used as a
    // directory prefix is never traversed, even when it points back inside the
    // repository. The only allowed literal-link fast path is an exact file,
    // whose canonical target must remain under the canonical repository root.
    let components = prefix.components().collect::<Vec<_>>();
    let mut candidate = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        candidate.push(component.as_os_str());
        let metadata = match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(_) => return Ok(WalkPlan::Empty),
        };
        let last = index + 1 == components.len();
        if metadata.file_type().is_symlink() {
            let target =
                canonical_contained_target(root, &candidate).map_err(|error| match error {
                    ContainmentError::Root(e) => {
                        invalid("glob", format!("cannot resolve repo root: {e}"))
                    }
                    ContainmentError::Target(e) => invalid(
                        "glob",
                        format!("cannot resolve literal path '{}': {e}", prefix.display()),
                    ),
                    ContainmentError::Outside => invalid(
                        "glob",
                        format!(
                            "literal path '{}' resolves outside the repository root",
                            prefix.display()
                        ),
                    ),
                })?;
            if !last || !whole_glob {
                // Directory-link prefixes are never traversed. We still resolve
                // them first so an outside-root target is rejected rather than
                // reported as an ordinary empty match.
                return Ok(WalkPlan::Empty);
            }
            return match target.metadata() {
                Ok(target_metadata) if target_metadata.is_file() => Ok(WalkPlan::SingleFile {
                    path: target,
                    relative: prefix,
                }),
                _ => Ok(WalkPlan::Empty),
            };
        }
        if !last && !metadata.is_dir() {
            return Ok(WalkPlan::Empty);
        }
        if last {
            return if metadata.is_dir() {
                Ok(WalkPlan::Subtree(candidate))
            } else if metadata.is_file() && whole_glob {
                Ok(WalkPlan::SingleFile {
                    path: candidate,
                    relative: prefix,
                })
            } else {
                Ok(WalkPlan::Empty)
            };
        }
    }
    Ok(WalkPlan::Empty)
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
fn grep_dir(
    root: &Path,
    regex: &regex::Regex,
    options: GrepScanOptions<'_>,
) -> Result<GrepScan, AppError> {
    let GrepScanOptions {
        overrides,
        glob,
        limit,
        started,
        deadline,
        cancelled,
        pool,
        threads,
    } = options;

    let mut candidates = Vec::new();
    let mut candidate_files = 0usize;
    let mut files_skipped = 0usize;
    let mut traversal_stop = None;
    match plan_walk(root, glob)? {
        WalkPlan::Empty => {}
        WalkPlan::SingleFile { path, relative } => {
            // The glob names exactly one real file — no traversal at all.
            candidate_files = 1;
            match path.metadata() {
                Ok(md) if md.len() <= GREP_MAX_FILE_BYTES => {
                    candidates.push(CandidateFile { path, relative });
                }
                _ => files_skipped = 1,
            }
        }
        plan => {
            let walk_root = match plan {
                WalkPlan::Subtree(dir) => dir,
                _ => root.to_path_buf(),
            };
            let mut builder = ignore::WalkBuilder::new(&walk_root);
            builder
                .hidden(false)
                .git_ignore(true)
                .git_exclude(true)
                .git_global(true)
                .add_custom_ignore_filename(".cihignore")
                .filter_entry(|entry| {
                    if entry.depth() > 0 && entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                    {
                        let name = entry.file_name().to_string_lossy();
                        return !GREP_SKIP_DIRS.contains(&name.as_ref());
                    }
                    true
                });
            if let Some(overrides) = overrides {
                builder.overrides(overrides);
            }

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
        }
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
    Ok(GrepScan {
        matches,
        complete: reason == GrepTruncationReason::None,
        truncation_reason: reason,
        candidate_files,
        files_scanned,
        files_skipped,
        elapsed_ms,
    })
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
                file: portable_relative_path(&candidate.relative),
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

    fn test_grep_result(
        root: &Path,
        pattern: &str,
        glob: &str,
        limit: usize,
    ) -> Result<GrepScan, AppError> {
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
                glob,
                limit,
                started,
                deadline: started + Duration::from_secs(5),
                cancelled: &cancelled,
                pool: &pool,
                threads: 2,
            },
        )
    }

    fn test_grep(root: &Path, pattern: &str, glob: &str, limit: usize) -> GrepScan {
        test_grep_result(root, pattern, glob, limit).unwrap()
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
    fn literal_walk_prefix_extracts_metachar_free_prefixes() {
        let p =
            |g: &str| literal_walk_prefix(g).map(|(p, whole)| (portable_relative_path(&p), whole));
        assert_eq!(
            p("src/main/App.java"),
            Some(("src/main/App.java".into(), true))
        );
        assert_eq!(
            p("./src/main/App.java"),
            Some(("src/main/App.java".into(), true))
        );
        assert_eq!(p("src/main/**/*.java"), Some(("src/main".into(), false)));
        assert_eq!(p("src/*.java"), Some(("src".into(), false)));
        assert_eq!(p("**/*.java"), None);
        assert_eq!(p("*.java"), None);
        assert_eq!(
            p("../src/App.java"),
            None,
            "parent-dir escapes are never literal"
        );
        assert_eq!(p("src/../App.java"), Some(("src".into(), false)));
        assert_eq!(p("/abs/path.java"), None);
    }

    /// A single-file glob must stat that one file — never walk the tree.
    #[test]
    fn grep_single_file_glob_takes_the_fast_path() {
        let root = grep_root("fastpath-file");
        write_under(&root, "src/main/App.java", b"// TODO one\n");
        write_under(&root, "src/other/Noise.java", b"// TODO noise\n");
        assert_eq!(
            plan_walk(&root, "src/main/App.java").unwrap(),
            WalkPlan::SingleFile {
                path: root.join("src/main/App.java"),
                relative: std::path::PathBuf::from("src/main/App.java"),
            }
        );
        let scan = test_grep(&root, "TODO", "src/main/App.java", 100);
        assert!(scan.complete);
        assert_eq!(
            scan.candidate_files, 1,
            "no traversal beyond the named file"
        );
        assert_eq!(scan.matches.len(), 1);
        assert_eq!(scan.matches[0].file, "src/main/App.java");
    }

    #[cfg(unix)]
    #[test]
    fn grep_single_file_symlink_is_allowed_only_for_an_in_repo_target() {
        use std::os::unix::fs::symlink;

        let root = grep_root("fastpath-contained-symlink");
        write_under(&root, "src/Target.java", b"// TODO contained\n");
        symlink("Target.java", root.join("src/Alias.java")).unwrap();

        let scan = test_grep(&root, "TODO", "src/Alias.java", 100);
        assert_eq!(scan.candidate_files, 1);
        assert_eq!(scan.files_scanned, 1);
        assert_eq!(scan.matches.len(), 1);
        assert_eq!(scan.matches[0].file, "src/Alias.java");
    }

    #[cfg(unix)]
    #[test]
    fn grep_single_file_symlink_rejects_an_outside_target() {
        use std::os::unix::fs::symlink;

        let root = grep_root("fastpath-escaping-symlink");
        let outside = std::env::temp_dir().join(format!(
            "cih-grepfiles-outside-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&outside, b"// TODO secret\n").unwrap();
        symlink(&outside, root.join("escape.java")).unwrap();

        let error = plan_walk(&root, "escape.java").unwrap_err();
        assert!(error.to_string().contains("outside the repository root"));
        std::fs::remove_file(outside).ok();
    }

    #[cfg(unix)]
    #[test]
    fn grep_does_not_traverse_a_directory_symlink_prefix() {
        use std::os::unix::fs::symlink;

        let root = grep_root("fastpath-directory-symlink");
        write_under(&root, "real/App.java", b"// TODO real\n");
        symlink("real", root.join("alias")).unwrap();

        assert_eq!(
            plan_walk(&root, "alias/**/*.java").unwrap(),
            WalkPlan::Empty
        );
        let scan = test_grep(&root, "TODO", "alias/**/*.java", 100);
        assert_eq!(scan.candidate_files, 0);
        assert!(scan.matches.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn grep_rejects_an_outside_root_directory_symlink_prefix() {
        use std::os::unix::fs::symlink;

        let root = grep_root("fastpath-escaping-directory-symlink");
        let outside = std::env::temp_dir().join(format!(
            "cih-grepfiles-outside-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_under(&outside, "Secret.java", b"// TODO secret\n");
        symlink(&outside, root.join("escape")).unwrap();

        let plan_error = plan_walk(&root, "escape/**/*.java").unwrap_err();
        assert!(plan_error
            .to_string()
            .contains("outside the repository root"));
        let grep_error = test_grep_result(&root, "TODO", "escape/**/*.java", 100).unwrap_err();
        assert!(grep_error
            .to_string()
            .contains("outside the repository root"));
        std::fs::remove_dir_all(outside).ok();
    }

    /// A literal directory prefix roots the walk at that subtree; match paths
    /// stay repo-relative.
    #[test]
    fn grep_literal_prefix_prunes_walk_to_subtree() {
        let root = grep_root("fastpath-subtree");
        write_under(&root, "src/main/App.java", b"// TODO in scope\n");
        write_under(&root, "vendor/big/Other.java", b"// TODO out of scope\n");
        assert!(matches!(
            plan_walk(&root, "src/main/**/*.java").unwrap(),
            WalkPlan::Subtree(_)
        ));
        let scan = test_grep(&root, "TODO", "src/main/**/*.java", 100);
        assert_eq!(scan.candidate_files, 1, "walk must not visit vendor/");
        assert_eq!(scan.matches.len(), 1);
        assert_eq!(scan.matches[0].file, "src/main/App.java");
    }

    /// A literal prefix that doesn't exist can't match anything — complete
    /// empty result, no walk.
    #[test]
    fn grep_missing_literal_prefix_returns_empty() {
        let root = grep_root("fastpath-missing");
        write_under(&root, "src/App.java", b"// TODO\n");
        assert_eq!(plan_walk(&root, "nope/**/*.java").unwrap(), WalkPlan::Empty);
        let scan = test_grep(&root, "TODO", "nope/**/*.java", 100);
        assert!(scan.complete);
        assert_eq!(scan.candidate_files, 0);
        assert!(scan.matches.is_empty());
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
                glob: "",
                limit: 100,
                started,
                deadline: started,
                cancelled: &cancelled,
                pool: &pool,
                threads: 1,
            },
        )
        .unwrap();
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

    #[test]
    fn grep_blocking_errors_name_their_tuning_knobs() {
        let timed_out = grep_blocking_error(BlockingError::TimedOut {
            label: "grep",
            secs: 90,
        });
        let timed_out = timed_out.to_string();
        assert!(timed_out.contains("CIH_GREP_DEADLINE_SECS"));
        assert!(timed_out.contains("CIH_GREP_QUEUE_TIMEOUT_SECS"));
        assert!(timed_out.contains("CIH_BLOCKING_TIMEOUT_SECS"));

        let saturated = grep_blocking_error(BlockingError::Saturated {
            label: "grep",
            waited_secs: 5,
        });
        let saturated = saturated.to_string();
        assert!(saturated.contains("CIH_BLOCKING_MAX_CONCURRENT"));
        assert!(saturated.contains("CIH_BLOCKING_QUEUE_TIMEOUT_SECS"));
    }

    fn containment_root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("cih-containment-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/App.java"), "class App {}\n").unwrap();
        root
    }

    #[test]
    fn contained_target_resolves_ordinary_files() {
        let root = containment_root("plain");
        let target = canonical_contained_target(&root, &root.join("src/App.java")).unwrap();
        assert!(target.ends_with("src/App.java"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn contained_target_reports_missing_and_outside_distinctly() {
        let root = containment_root("errors");
        assert!(matches!(
            canonical_contained_target(&root, &root.join("src/Missing.java")),
            Err(ContainmentError::Target(_))
        ));

        let outside = std::env::temp_dir().join(format!(
            "cih-containment-outside-{}.java",
            std::process::id()
        ));
        std::fs::write(&outside, "class Outside {}\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&outside, root.join("src/Escape.java")).unwrap();
            assert!(matches!(
                canonical_contained_target(&root, &root.join("src/Escape.java")),
                Err(ContainmentError::Outside)
            ));
            // A symlink to an in-repo target resolves to the contained file.
            symlink(root.join("src/App.java"), root.join("src/Alias.java")).unwrap();
            let aliased = canonical_contained_target(&root, &root.join("src/Alias.java")).unwrap();
            assert!(aliased.ends_with("src/App.java"));
        }
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn read_span_slices_lines_without_numbering() {
        let root = containment_root("span");
        std::fs::write(root.join("src/App.java"), "a\nb\nc\nd\ne\n").unwrap();
        let span = read_span(
            &root.join("src/App.java"),
            SourceSpanCommand {
                path: "src/App.java".into(),
                start_line: 2,
                end_line: 4,
                max_lines: 120,
                max_bytes: 8 * 1024,
            },
        )
        .unwrap();
        assert_eq!(span.content, "b\nc\nd");
        assert_eq!((span.start_line, span.end_line), (2, 4));
        assert!(!span.truncated);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn read_span_caps_lines_and_bytes_on_char_boundaries() {
        let root = containment_root("span-caps");
        std::fs::write(root.join("src/App.java"), "one\ntwo\nthree\nfour\n").unwrap();
        let line_capped = read_span(
            &root.join("src/App.java"),
            SourceSpanCommand {
                path: "src/App.java".into(),
                start_line: 1,
                end_line: 4,
                max_lines: 2,
                max_bytes: 8 * 1024,
            },
        )
        .unwrap();
        assert_eq!(line_capped.content, "one\ntwo");
        assert!(line_capped.truncated);

        // Multi-byte content must be cut on a character boundary.
        std::fs::write(root.join("src/App.java"), "héllo wörld ünïcode\n").unwrap();
        let byte_capped = read_span(
            &root.join("src/App.java"),
            SourceSpanCommand {
                path: "src/App.java".into(),
                start_line: 1,
                end_line: 1,
                max_lines: 120,
                max_bytes: 8,
            },
        )
        .unwrap();
        assert!(byte_capped.truncated);
        assert!(byte_capped.content.len() <= 8);
        assert!(byte_capped
            .content
            .is_char_boundary(byte_capped.content.len()));
        assert!(byte_capped.content.starts_with("héllo"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn read_span_counts_blank_line_separators_against_byte_cap() {
        let root = containment_root("span-blank-byte-cap");
        std::fs::write(root.join("src/App.java"), "abc\n\n\n").unwrap();

        let exhausted = read_span(
            &root.join("src/App.java"),
            SourceSpanCommand {
                path: "src/App.java".into(),
                start_line: 1,
                end_line: 3,
                max_lines: 120,
                max_bytes: 3,
            },
        )
        .unwrap();
        assert_eq!(exhausted.content, "abc");
        assert!(exhausted.truncated);
        assert!(exhausted.content.len() <= 3);

        let one_blank = read_span(
            &root.join("src/App.java"),
            SourceSpanCommand {
                path: "src/App.java".into(),
                start_line: 1,
                end_line: 3,
                max_lines: 120,
                max_bytes: 4,
            },
        )
        .unwrap();
        assert_eq!(one_blank.content, "abc\n");
        assert_eq!(one_blank.end_line, 2);
        assert!(one_blank.truncated);
        assert!(one_blank.content.len() <= 4);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn char_boundary_prefix_never_splits_characters() {
        assert_eq!(char_boundary_prefix("héllo", 2), "h");
        assert_eq!(char_boundary_prefix("héllo", 3), "hé");
        assert_eq!(char_boundary_prefix("héllo", 100), "héllo");
        assert_eq!(char_boundary_prefix("héllo", 0), "");
    }
}
