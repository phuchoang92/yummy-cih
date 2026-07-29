//! FalkorDB `GraphStore` + `BulkLoader` adapter (the open-source / dev backend).
//!
//! FalkorDB speaks the Redis protocol with a `GRAPH.QUERY <key> <openCypher>`
//! command, driven here via the `redis` crate. At go-live the same openCypher
//! queries move to a Neptune adapter (different driver: HTTPS + SigV4).
//!
//! All nodes carry a single `:Symbol` label + a `kind` property; the node `id`
//! is unique and indexed. Read queries pass the id via the FalkorDB `CYPHER`
//! parameter preamble (`CYPHER id=<lit> ... $id ...`) so the plan is cached and
//! the id is not concatenated ad-hoc into the match pattern. Bulk writes inline a
//! generated `UNWIND` list literal (our own data, fully escaped).

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use cih_core::{Edge, EdgeKind, GraphArtifacts, Node, NodeId};
use cih_graph_store::{
    BackendReadiness, ContextCursorKey, ContextSection, Direction, GraphStore, GraphStoreError,
    LoadObserver, LoadStats, Result, RetryMetadata,
};
use redis::Value;

use serialize::*;

mod bulk;
mod query;
mod serialize;

/// Rows per UNWIND batch during bulk load. Larger batches cut Redis round-trips on big graphs
/// (~2M edges at 600k nodes) at the cost of bigger per-statement strings — 4000 is a good balance.
const BATCH: usize = 4000;

/// Required indexed lookups for every supported graph publication path.
/// Schema setup, Cypher upsert completion, and native-bulk completion all use
/// this one list so a new hot lookup cannot silently miss one loader.
/// The numeric complexity properties back `complexity_hotspots`' range
/// predicates — without them the tool degree-scans every Method/Constructor
/// and times out on million-node graphs.
const REQUIRED_SYMBOL_INDEXES: [&str; 7] = [
    "id",
    "kind",
    "name",
    "file",
    "cyclomatic",
    "cognitive",
    "transitiveLoopDepth",
];

/// Default max wait for a query permit before shedding (used when no explicit
/// limit is configured, e.g. the engine bulk-load path).
const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);
const TRANSIENT_RETRY_AFTER_MS: u64 = 1_000;
/// After the client-facing driver deadline, retain admission until the Redis
/// future observes FalkorDB's earlier server timeout. A stuck driver task is
/// aborted after this grace so one lost socket cannot leak capacity forever.
const READ_CLEANUP_GRACE: Duration = Duration::from_secs(2);
const READ_LOAD_WAIT_SETTING: &str = "CIH_FALKOR_READ_LOAD_WAIT_SECS";
const WRITE_LOAD_WAIT_SETTING: &str = "CIH_FALKOR_LOAD_WAIT_SECS";

#[derive(Debug)]
enum SupervisedReadError<E> {
    Operation(E),
    TimedOut,
    TaskFailed(String),
}

/// Owns the detached backend task. Dropping the caller future—whether because
/// its timeout fired or its client disconnected—moves the task into a bounded
/// reaper instead of releasing its query permit immediately.
struct ReadTaskGuard {
    task: Option<tokio::task::JoinHandle<()>>,
    cleanup_grace: Duration,
}

impl ReadTaskGuard {
    async fn finish(mut self) -> std::result::Result<(), String> {
        let task = self.task.take().expect("read task guard owns a task");
        task.await.map_err(|error| error.to_string())
    }
}

impl Drop for ReadTaskGuard {
    fn drop(&mut self) {
        let Some(mut task) = self.task.take() else {
            return;
        };
        let cleanup_grace = self.cleanup_grace;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if tokio::time::timeout(cleanup_grace, &mut task)
                    .await
                    .is_err()
                {
                    task.abort();
                    let _ = task.await;
                }
            });
        } else {
            task.abort();
        }
    }
}

async fn run_supervised_read<T, E, F>(
    permit: tokio::sync::OwnedSemaphorePermit,
    driver_timeout: Duration,
    cleanup_grace: Duration,
    operation: F,
) -> std::result::Result<T, SupervisedReadError<E>>
where
    T: Send + 'static,
    E: Send + 'static,
    F: Future<Output = std::result::Result<T, E>> + Send + 'static,
{
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _permit = permit;
        let result = operation.await;
        let _ = result_tx.send(result);
    });
    let guard = ReadTaskGuard {
        task: Some(task),
        cleanup_grace,
    };

    match tokio::time::timeout(driver_timeout, result_rx).await {
        Ok(Ok(result)) => {
            guard
                .finish()
                .await
                .map_err(SupervisedReadError::TaskFailed)?;
            result.map_err(SupervisedReadError::Operation)
        }
        Ok(Err(_)) => {
            let message = guard
                .finish()
                .await
                .err()
                .unwrap_or_else(|| "result channel closed before completion".to_string());
            Err(SupervisedReadError::TaskFailed(message))
        }
        Err(_) => Err(SupervisedReadError::TimedOut),
    }
}

pub struct FalkorStore {
    client: redis::Client,
    graph_key: String,
    /// Lazily-built, auto-reconnecting, cloneable multiplexed connection. Built
    /// once on first query and shared across all calls — replaces opening a
    /// fresh connection per query.
    conn: tokio::sync::OnceCell<redis::aio::ConnectionManager>,
    /// Bounds concurrent `GRAPH.QUERY` execution (backpressure). Defaults to
    /// effectively unlimited; the server tightens it via [`Self::with_query_limit`].
    query_limit: Arc<tokio::sync::Semaphore>,
    /// Max time to wait for a permit before shedding with an "overloaded" error.
    acquire_timeout: Duration,
    /// FalkorDB's server-side limit for read-only `GRAPH.QUERY` execution.
    /// Writes and bulk loads intentionally do not use this deadline.
    read_backend_timeout: Option<Duration>,
    /// Client-side deadline around the Redis read query future. This should be
    /// longer than `read_backend_timeout` so FalkorDB can return its own timeout.
    read_driver_timeout: Option<Duration>,
    /// Optional composition-root override for the compatibility loading wait.
    /// A zero duration makes server reads return typed `Loading` immediately;
    /// CLI callers that do not inject it retain the legacy environment policy.
    read_load_wait: Option<Duration>,
}

impl FalkorStore {
    pub fn connect(url: &str, graph_key: impl Into<String>) -> Result<Self> {
        let client = redis::Client::open(url).map_err(|e| Self::map_redis_error(e, None))?;
        Ok(Self {
            client,
            graph_key: graph_key.into(),
            conn: tokio::sync::OnceCell::new(),
            // Effectively unlimited by default — the engine's sequential bulk-load
            // path must never be throttled. The server opts into a real bound.
            query_limit: Arc::new(tokio::sync::Semaphore::new(
                tokio::sync::Semaphore::MAX_PERMITS,
            )),
            acquire_timeout: DEFAULT_ACQUIRE_TIMEOUT,
            read_backend_timeout: None,
            read_driver_timeout: None,
            read_load_wait: None,
        })
    }

    /// Bound concurrent Cypher queries to `max_concurrent`, shedding requests
    /// that can't acquire a permit within `acquire_timeout`. Used by the MCP
    /// server to apply backpressure under multi-client load.
    pub fn with_query_limit(mut self, max_concurrent: usize, acquire_timeout: Duration) -> Self {
        self.query_limit = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
        self.acquire_timeout = acquire_timeout;
        self
    }

    /// Apply two independent deadlines to read queries only.
    ///
    /// `backend_timeout` is sent to FalkorDB as `GRAPH.QUERY ... TIMEOUT <ms>`;
    /// `driver_timeout` bounds the Redis future and must be longer so the server
    /// normally gets the first opportunity to cancel expensive graph work.
    pub fn with_read_timeouts(
        mut self,
        backend_timeout: Duration,
        driver_timeout: Duration,
    ) -> Result<Self> {
        if backend_timeout.is_zero() {
            return Err(GraphStoreError::InvalidInput(
                "FalkorDB read backend timeout must be greater than zero".into(),
            ));
        }
        if driver_timeout <= backend_timeout {
            return Err(GraphStoreError::InvalidInput(format!(
                "FalkorDB read driver timeout ({driver_timeout:?}) must be greater than the \
                 backend timeout ({backend_timeout:?})"
            )));
        }
        self.read_backend_timeout = Some(backend_timeout);
        self.read_driver_timeout = Some(driver_timeout);
        Ok(self)
    }

    /// Override the legacy per-read loading wait. Server composition roots use
    /// zero and rely on their cached readiness monitor; CLI callers keep the
    /// compatibility backstop when this is not set.
    pub fn with_read_load_wait(mut self, wait: Duration) -> Self {
        self.read_load_wait = Some(wait);
        self
    }

    /// Per-connection-attempt timeout for establishing the FalkorDB connection,
    /// overridable via `CIH_FALKOR_CONNECT_TIMEOUT_SECS` (default 5s). Bounds the
    /// initial connect so a **down or firewalled** FalkorDB fails fast instead of
    /// blocking on the OS TCP timeout; distinct from `load_wait_budget` (600s),
    /// which waits out a reachable-but-still-loading instance at the query level.
    fn connect_timeout() -> Duration {
        Duration::from_secs(
            std::env::var("CIH_FALKOR_CONNECT_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&s| s > 0)
                .unwrap_or(5),
        )
    }

    /// A clone of the shared, reconnecting connection, building it on first use.
    ///
    /// A down/firewalled host would otherwise block for ~a minute: `new_with_config`
    /// feeds redis's default retry `factor` (100) into an exponential backoff, so
    /// even a couple of retries sleep far longer than the per-attempt
    /// `connection_timeout`. We therefore (a) tighten the backoff (`factor`/`max_delay`)
    /// and cap each attempt, and (b) wrap the **initial** establishment in a total
    /// `connect_timeout()` so a truly-down instance surfaces an error in seconds.
    /// Only the first connect is time-boxed here — the manager's later transparent
    /// reconnects (which the long-running server depends on) keep their own logic.
    /// A reachable-but-loading instance is unaffected: TCP connect succeeds while
    /// queries return `BusyLoadingError`, which `wait_until_ready` handles.
    async fn conn(&self) -> Result<redis::aio::ConnectionManager> {
        self.conn
            .get_or_try_init(|| async {
                let budget = Self::connect_timeout();
                let config = redis::aio::ConnectionManagerConfig::new()
                    .set_connection_timeout(budget)
                    .set_factor(2)
                    .set_max_delay(500);
                match tokio::time::timeout(
                    budget,
                    redis::aio::ConnectionManager::new_with_config(self.client.clone(), config),
                )
                .await
                {
                    Ok(res) => res,
                    Err(_) => Err(redis::RedisError::from((
                        redis::ErrorKind::IoError,
                        "FalkorDB connection timed out",
                        format!(
                            "could not connect within {}s — is FalkorDB running? \
                             (raise CIH_FALKOR_CONNECT_TIMEOUT_SECS to wait longer)",
                            budget.as_secs()
                        ),
                    ))),
                }
            })
            .await
            .cloned()
            .map_err(|e| Self::map_redis_error(e, None))
    }

    fn backend_readiness_command() -> redis::Cmd {
        let mut command = redis::cmd("INFO");
        command.arg("persistence");
        command
    }

    /// Inspect Redis restore state on the metadata lane. This deliberately
    /// bypasses the interactive GRAPH.QUERY semaphore and never touches the
    /// configured graph key, so probing cannot create an empty graph or run
    /// schema DDL.
    async fn probe_backend_readiness(&self) -> Result<BackendReadiness> {
        let mut connection = self.conn().await?;
        let info = Self::backend_readiness_command()
            .query_async::<String>(&mut connection)
            .await
            .map_err(|error| Self::map_redis_error(error, None))?;
        Self::parse_backend_readiness(&info)
    }

    fn parse_backend_readiness(info: &str) -> Result<BackendReadiness> {
        let field = |name: &str| {
            info.lines().find_map(|line| {
                let (key, value) = line.trim_end_matches('\r').split_once(':')?;
                (key == name).then_some(value.trim())
            })
        };

        match field("loading") {
            Some("0") => Ok(BackendReadiness::ready()),
            Some("1") => {
                let detail = field("loading_loaded_perc").map_or_else(
                    || "Redis/FalkorDB is restoring its persisted dataset".to_string(),
                    |percentage| {
                        format!(
                            "Redis/FalkorDB is restoring its persisted dataset ({percentage}% loaded)"
                        )
                    },
                );
                Ok(BackendReadiness::loading(detail, TRANSIENT_RETRY_AFTER_MS))
            }
            Some(value) => Err(GraphStoreError::Unavailable {
                message: format!("Redis INFO persistence returned invalid loading={value}"),
                retry: RetryMetadata::non_retryable(),
            }),
            None => Err(GraphStoreError::Unavailable {
                message: "Redis INFO persistence omitted the loading field".into(),
                retry: RetryMetadata::non_retryable(),
            }),
        }
    }

    /// Acquire a query permit, shedding with an "overloaded" error if the
    /// concurrency limit is saturated for longer than `acquire_timeout`.
    async fn acquire_permit(&self) -> Result<tokio::sync::OwnedSemaphorePermit> {
        match tokio::time::timeout(
            self.acquire_timeout,
            self.query_limit.clone().acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => Ok(permit),
            // Semaphore was closed — the store has been dropped; retrying is futile.
            Ok(Err(_)) => Err(GraphStoreError::Unavailable {
                message: "query semaphore closed".into(),
                retry: RetryMetadata::non_retryable(),
            }),
            // Timeout elapsed — transient overload, caller may retry.
            Err(_) => Err(GraphStoreError::Overloaded {
                message: format!(
                    "concurrent query limit remained saturated for {}ms",
                    duration_millis(self.acquire_timeout)
                ),
                retry: RetryMetadata::retryable(Some(duration_millis(self.acquire_timeout))),
            }),
        }
    }

    async fn run(&self, cypher: &str) -> Result<Value> {
        let _permit = self.acquire_permit().await?;
        let mut con = self.conn().await?;
        redis::cmd("GRAPH.QUERY")
            .arg(&self.graph_key)
            .arg(cypher)
            .query_async(&mut con)
            .await
            .map_err(|e| Self::map_redis_error(e, None))
    }

    fn read_query_command(&self, cypher: &str) -> redis::Cmd {
        let mut command = redis::cmd("GRAPH.QUERY");
        command.arg(&self.graph_key).arg(cypher);
        if let Some(timeout) = self.read_backend_timeout {
            command.arg("TIMEOUT").arg(duration_millis(timeout));
        }
        command
    }

    /// Execute one read attempt. Unlike [`Self::run`], this injects both
    /// server- and driver-side timeouts and is never used by writes/bulk load.
    async fn run_read_once(&self, cypher: &str) -> Result<Value> {
        let permit = self.acquire_permit().await?;
        let mut con = self.conn().await?;
        let command = self.read_query_command(cypher);
        let backend_timeout = self.read_backend_timeout;

        let result = if let Some(driver_timeout) = self.read_driver_timeout {
            let operation = async move { command.query_async::<Value>(&mut con).await };
            match run_supervised_read(permit, driver_timeout, READ_CLEANUP_GRACE, operation).await {
                Ok(value) => Ok(value),
                Err(SupervisedReadError::Operation(error)) => Err(error),
                Err(SupervisedReadError::TimedOut) => {
                    return Err(GraphStoreError::ExecutionTimeout {
                        message: format!(
                            "Redis driver deadline elapsed before FalkorDB replied; query capacity remains reserved for up to {}ms of cleanup",
                            duration_millis(READ_CLEANUP_GRACE)
                        ),
                        timeout_ms: duration_millis(driver_timeout),
                        retry: RetryMetadata::non_retryable(),
                    });
                }
                Err(SupervisedReadError::TaskFailed(message)) => {
                    return Err(GraphStoreError::Unavailable {
                        message: format!("Redis read supervisor failed: {message}"),
                        retry: RetryMetadata::retryable(Some(TRANSIENT_RETRY_AFTER_MS)),
                    });
                }
            }
        } else {
            let _permit = permit;
            command.query_async::<Value>(&mut con).await
        };

        result.map_err(|error| Self::map_redis_error(error, backend_timeout))
    }

    async fn graph_command(&self, command: &str, args: &[&str]) -> Result<Value> {
        let _permit = self.acquire_permit().await?;
        let mut con = self.conn().await?;
        let mut cmd = redis::cmd(command);
        for arg in args {
            cmd.arg(arg);
        }
        cmd.query_async(&mut con)
            .await
            .map_err(|e| Self::map_redis_error(e, None))
    }

    /// Max time a READ waits out a loading FalkorDB before failing, from
    /// `CIH_FALKOR_READ_LOAD_WAIT_SECS` (default 20s — bounded, unlike the
    /// write budget: a read caller is interactive and can retry).
    fn read_load_wait_budget() -> Duration {
        Duration::from_secs(
            std::env::var("CIH_FALKOR_READ_LOAD_WAIT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(20),
        )
    }

    /// Run a read query, waiting out a `BusyLoadingError` once within the
    /// (short) read budget. Without this, every read during a restart's
    /// dataset load surfaced as "graph store unavailable" with no retry —
    /// writes already waited (`run_write`), reads did not.
    async fn run_read(&self, cypher: &str) -> Result<Value> {
        match self.run_read_once(cypher).await {
            Err(e) if Self::is_loading_error(&e) => {
                let wait = self
                    .read_load_wait
                    .unwrap_or_else(Self::read_load_wait_budget);
                if wait.is_zero() {
                    return Err(e);
                }
                self.wait_until_ready(wait, READ_LOAD_WAIT_SETTING).await?;
                self.run_read_once(cypher).await
            }
            other => other,
        }
    }

    /// Result rows (the second element of a GRAPH.QUERY reply) as stringified
    /// cells. Good enough for scalar `RETURN` columns.
    async fn rows(&self, cypher: &str) -> Result<Vec<Vec<String>>> {
        let reply = self.run_read(cypher).await?;
        let top = as_array(&reply);
        let Some(rows_val) = top.get(1) else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        for row in as_array(rows_val) {
            out.push(as_array(row).iter().map(cell_to_string).collect());
        }
        Ok(out)
    }

    /// True when a backend error is FalkorDB/Redis reporting it is still loading
    /// its persisted dataset into memory (`BusyLoadingError`). Transient at
    /// startup — especially when one instance holds a large (multi-GB) AOF/RDB.
    fn is_loading_error(e: &GraphStoreError) -> bool {
        matches!(e, GraphStoreError::Loading { .. })
    }

    fn is_query_timeout_error(error: &redis::RedisError) -> bool {
        let code = error.code();
        let detail = error.detail();
        code.is_some_and(|code| code.eq_ignore_ascii_case("TIMEOUT"))
            // FalkorDB may emit `-Query timed out`; Redis then parses `Query`
            // as the extension code and `timed out` as its detail.
            || (code.is_some_and(|code| code.eq_ignore_ascii_case("QUERY"))
                && detail.is_some_and(|detail| detail.eq_ignore_ascii_case("timed out")))
            || detail.is_some_and(|detail| {
                let detail = detail.trim();
                detail.eq_ignore_ascii_case("query timed out")
                    || detail.eq_ignore_ascii_case("query timeout")
            })
    }

    /// Preserve Redis's structured error kind instead of flattening it into a
    /// string. In particular, `BusyLoadingError` is the only condition mapped to
    /// [`GraphStoreError::Loading`]; arbitrary messages containing "loading" do
    /// not trigger readiness polling.
    fn map_redis_error(
        error: redis::RedisError,
        execution_timeout: Option<Duration>,
    ) -> GraphStoreError {
        let kind = error.kind();
        let message = error.to_string();

        if kind == redis::ErrorKind::BusyLoadingError {
            return GraphStoreError::Loading {
                message,
                retry: RetryMetadata::retryable(Some(TRANSIENT_RETRY_AFTER_MS)),
            };
        }
        if Self::is_query_timeout_error(&error) {
            return GraphStoreError::ExecutionTimeout {
                message,
                timeout_ms: execution_timeout.map(duration_millis).unwrap_or_default(),
                // Re-running the same query under the same deadline usually
                // repeats the timeout; the caller should narrow/degrade it.
                retry: RetryMetadata::non_retryable(),
            };
        }

        match kind {
            redis::ErrorKind::TryAgain
            | redis::ErrorKind::ClusterDown
            | redis::ErrorKind::MasterDown
            | redis::ErrorKind::IoError
            | redis::ErrorKind::ClusterConnectionNotFound
            | redis::ErrorKind::MasterNameNotFoundBySentinel
            | redis::ErrorKind::NoValidReplicasFoundBySentinel => GraphStoreError::Unavailable {
                message,
                retry: RetryMetadata::retryable(Some(TRANSIENT_RETRY_AFTER_MS)),
            },
            redis::ErrorKind::AuthenticationFailed
            | redis::ErrorKind::InvalidClientConfig
            | redis::ErrorKind::ReadOnly
            | redis::ErrorKind::EmptySentinelList => GraphStoreError::Unavailable {
                message,
                retry: RetryMetadata::non_retryable(),
            },
            _ => GraphStoreError::Backend(message),
        }
    }

    /// Ensure one of the fixed `Symbol` indexes exists. A concurrent or
    /// repeated CREATE is accepted only after `db.indexes()` confirms the exact
    /// label/property pair; every other DDL failure is surfaced.
    async fn ensure_symbol_index(&self, property: &'static str) -> Result<()> {
        let ddl = format!("CREATE INDEX FOR (n:Symbol) ON (n.{property})");
        match self.run(&ddl).await {
            Ok(_) => Ok(()),
            Err(error) if Self::is_possible_duplicate_index_error(&error) => {
                match self.symbol_index_exists(property).await {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(Self::index_error(
                        property,
                        format!(
                            "backend reported an existing index, but db.indexes() did not \
                             contain :Symbol({property}): {error}"
                        ),
                        error.retry_metadata(),
                    )),
                    Err(verification_error) => Err(Self::index_error(
                        property,
                        format!(
                            "could not verify a possible duplicate index after `{ddl}` failed: \
                             create error: {error}; verification error: {verification_error}"
                        ),
                        verification_error
                            .retry_metadata()
                            .or_else(|| error.retry_metadata()),
                    )),
                }
            }
            // Preserve classified operational failures so callers retain their
            // retry guidance. Unclassified DDL errors become typed index errors.
            Err(error) if error.retry_metadata().is_some() => Err(error),
            Err(error) => Err(Self::index_error(
                property,
                format!("`{ddl}` failed: {error}"),
                None,
            )),
        }
    }

    async fn ensure_required_symbol_indexes(&self) -> Result<()> {
        for property in REQUIRED_SYMBOL_INDEXES {
            self.ensure_symbol_index(property).await?;
        }
        Ok(())
    }

    fn is_possible_duplicate_index_error(error: &GraphStoreError) -> bool {
        let GraphStoreError::Backend(message) = error else {
            return false;
        };
        let message = message.to_ascii_lowercase();
        message.contains("index")
            && (message.contains("already exists") || message.contains("already indexed"))
    }

    async fn symbol_index_exists(&self, property: &str) -> Result<bool> {
        Ok(self.rows("CALL db.indexes()").await?.iter().any(|row| {
            row.iter().any(|cell| cell_has_token(cell, "Symbol"))
                && row.iter().any(|cell| cell_has_token(cell, property))
        }))
    }

    fn index_error(
        property: &str,
        message: String,
        retry: Option<RetryMetadata>,
    ) -> GraphStoreError {
        GraphStoreError::Index {
            message: format!("failed to ensure :Symbol({property}) index: {message}"),
            retry: retry.unwrap_or_else(RetryMetadata::non_retryable),
        }
    }

    /// Max time to wait for a loading FalkorDB before giving up, from
    /// `CIH_FALKOR_LOAD_WAIT_SECS` (default 600s — enough for a multi-GB reload).
    fn load_wait_budget() -> Duration {
        Duration::from_secs(
            std::env::var("CIH_FALKOR_LOAD_WAIT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600),
        )
    }

    /// Block until FalkorDB can serve a trivial query, tolerating the
    /// `BusyLoadingError` window while it loads a large dataset. Polls with
    /// exponential backoff (capped at 5s) up to `max_wait`, logging progress so a
    /// multi-minute wait is visible. Any non-loading error fails fast.
    async fn wait_until_ready(&self, max_wait: Duration, wait_setting: &'static str) -> Result<()> {
        let start = std::time::Instant::now();
        let mut delay = Duration::from_millis(200);
        let mut next_log = Duration::from_secs(5);
        loop {
            match self.run("RETURN 1").await {
                Ok(_) => return Ok(()),
                Err(e) if Self::is_loading_error(&e) => {
                    if start.elapsed() >= max_wait {
                        return Err(GraphStoreError::Loading {
                            message: format!(
                                "FalkorDB still loading its dataset after {}s — artifacts are on \
                                 disk; re-run once it is ready, or raise {wait_setting}",
                                max_wait.as_secs()
                            ),
                            retry: RetryMetadata::retryable(Some(5_000)),
                        });
                    }
                    if start.elapsed() >= next_log {
                        tracing::info!(
                            elapsed_s = start.elapsed().as_secs(),
                            "waiting for FalkorDB to finish loading its dataset…"
                        );
                        next_log += Duration::from_secs(15);
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Run a write query, waiting out a `BusyLoadingError` (e.g. FalkorDB
    /// restarted mid-load) once before retrying — so a batch never fails just
    /// because the instance was momentarily loading.
    async fn run_write(&self, cypher: &str) -> Result<Value> {
        match self.run(cypher).await {
            Err(e) if Self::is_loading_error(&e) => {
                self.wait_until_ready(Self::load_wait_budget(), WRITE_LOAD_WAIT_SETTING)
                    .await?;
                self.run(cypher).await
            }
            other => other,
        }
    }

    /// Core write path: MERGE nodes then edges in UNWIND batches. Idempotent
    /// (re-running the same artifact is a no-op), so it doubles as upsert.
    async fn load_nodes_edges(
        &self,
        nodes: &[Node],
        edges: &[Edge],
        obs: &dyn LoadObserver,
    ) -> Result<LoadStats> {
        // Index the MERGE key before the Cypher batches. Duplicate-index errors
        // are accepted only after exact `db.indexes()` verification; loading and
        // unexpected DDL failures invalidate staging.
        self.ensure_symbol_index("id").await?;

        let node_chunks: Vec<_> = nodes.chunks(BATCH).collect();
        let total_node_batches = node_chunks.len();
        for (batch_idx, chunk) in node_chunks.into_iter().enumerate() {
            // ON CREATE SET only: the staging graph is always drop_graph()'d before
            // bulk_load runs, and upsert_incremental DETACH-DELETEs changed-file nodes
            // before calling load_nodes_edges. So every MERGE here creates a new node.
            // In the retry case (partial batch failure), matched nodes already hold the
            // correct values from the same artifact — skipping ON MATCH SET is safe and
            // avoids re-writing ~18 properties per node for zero net change.
            let q = format!(
                "UNWIND {arr} AS row \
                 MERGE (n:Symbol {{id: row.id}}) \
                 ON CREATE SET n.name = row.name, n.kind = row.kind, n.file = row.file, \
                     n.qualifiedName = row.qn, n.startLine = row.sl, n.endLine = row.el, \
                     n.props = row.props, n.stereotype = row.stereotype, \
                     n.httpMethod = row.httpMethod, n.path = row.path, \
                     n.decorator = row.decorator, n.handler = row.handler, \
                     n.symbolCount = row.symbolCount, n.cohesion = row.cohesion, \
                     n.processType = row.processType, \
                     n.cyclomatic = row.cyclomatic, n.cognitive = row.cognitive, \
                     n.loopDepth = row.loopDepth, n.transitiveLoopDepth = row.transitiveLoopDepth, \
                     n.isAccessor = row.isAccessor",
                arr = nodes_to_list(chunk)
            );
            self.run_write(&q).await.inspect_err(|_| {
                tracing::error!(
                    batch = batch_idx,
                    committed_batches = batch_idx,
                    total_batches = total_node_batches,
                    "node batch write failed — graph is partially written; \
                     re-run bulk_load from scratch to restore consistency"
                );
            })?;
        }

        obs.nodes_loaded(nodes.len() as u64);

        // Relationship types can't be parameterized in MERGE → one batch per kind.
        let mut by_kind: HashMap<EdgeKind, Vec<&Edge>> = HashMap::new();
        for e in edges {
            by_kind.entry(e.kind).or_default().push(e);
        }
        for (kind, es) in &by_kind {
            let label = kind.cypher_label();
            let edge_chunks: Vec<_> = es.chunks(BATCH).collect();
            let total_edge_batches = edge_chunks.len();
            for (batch_idx, chunk) in edge_chunks.into_iter().enumerate() {
                let q = format!(
                    "UNWIND {arr} AS row \
                     MATCH (a:Symbol {{id: row.src}}), (b:Symbol {{id: row.dst}}) \
                     MERGE (a)-[r:{label}]->(b) \
                     ON CREATE SET r.confidence = row.conf, r.reason = row.reason, \
                         r.callSites = row.callSites",
                    arr = edges_to_list(chunk)
                );
                self.run_write(&q).await.inspect_err(|_| {
                    tracing::error!(
                        kind = ?kind,
                        batch = batch_idx,
                        committed_batches = batch_idx,
                        total_batches = total_edge_batches,
                        "edge batch write failed — graph is partially written; \
                         re-run bulk_load from scratch to restore consistency"
                    );
                })?;
            }
        }
        obs.edges_loaded(edges.len() as u64);
        self.ensure_required_symbol_indexes().await?;
        obs.indexes_built();

        Ok(LoadStats {
            nodes: nodes.len() as u64,
            edges: edges.len() as u64,
        })
    }

    /// True when the graph holds no nodes — either it does not exist, or it is an
    /// empty graph key auto-created by a `GRAPH.QUERY` (e.g. the readiness probe
    /// in `wait_until_ready`, which is why an `EXISTS` check is not enough). Used
    /// to route `bulk_load` between the native `GRAPH.BULK` path and the Cypher
    /// upsert.
    async fn graph_is_empty(&self) -> Result<bool> {
        let rows = self
            .rows("MATCH (n) RETURN count(n) AS c")
            .await
            .unwrap_or_default();
        let count: u64 = rows
            .first()
            .and_then(|r| r.first())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Ok(count == 0)
    }

    /// Load a fresh graph via FalkorDB's native binary bulk-insert protocol
    /// (`GRAPH.BULK`). Skips the Cypher parser and per-edge `MATCH`; produces the
    /// same graph as the Cypher path (see `bulk::build_node_batches` /
    /// `build_edge_batches` for parity). Artifacts are **streamed** line-by-line
    /// and encoded into byte-budgeted batches — the node pass is sent and freed
    /// before the edge pass, so struct `Vec`s never exist and no single call
    /// approaches the 512 MB / 1 GB limits. The `id`/`kind` indexes are created
    /// *after* the insert — `BEGIN` requires an unused key.
    async fn bulk_insert(
        &self,
        artifacts: &GraphArtifacts,
        obs: &dyn LoadObserver,
    ) -> Result<LoadStats> {
        // `GRAPH.BULK BEGIN` requires an unused key. The readiness probe and the
        // emptiness check both auto-create an empty graph key, so drop it first.
        let _ = self.drop_graph().await;
        let budget = bulk::batch_budget();
        let read_err = |e: std::io::Error| GraphStoreError::Backend(format!("read artifacts: {e}"));

        // Node pass: stream + encode, keeping only the id→ordinal map.
        let t_load = std::time::Instant::now();
        let node_stream = artifacts.stream_nodes().map_err(read_err)?;
        let (node_batches, ordinal) =
            bulk::build_node_batches(node_stream, budget).map_err(read_err)?;
        let total_nodes = node_batches.iter().map(|(n, _)| n).sum::<u64>();
        if total_nodes == 0 {
            return Ok(LoadStats { nodes: 0, edges: 0 });
        }
        let mut payload_bytes: usize = node_batches.iter().map(|(_, b)| b.len()).sum();
        let mut call_count = node_batches.len();

        let mut con = {
            let _permit = self.acquire_permit().await?;
            let mut con = self.conn().await?;
            // Node batches first — they assign ordinals 0..N in send order.
            let mut first = true;
            for (nc, blob) in &node_batches {
                let mut cmd = redis::cmd("GRAPH.BULK");
                cmd.arg(&self.graph_key);
                if std::mem::take(&mut first) {
                    cmd.arg("BEGIN");
                }
                cmd.arg(*nc).arg(0).arg(1).arg(0).arg(blob.as_slice());
                let _reply: Value = cmd
                    .query_async(&mut con)
                    .await
                    .map_err(|e| GraphStoreError::Backend(format!("GRAPH.BULK nodes: {e}")))?;
            }
            con
        };
        drop(node_batches); // free node payload before encoding edges
        obs.nodes_loaded(total_nodes);

        // Edge pass: stream + encode against the ordinal map, then send.
        let edge_stream = artifacts.stream_edges().map_err(read_err)?;
        let edge_batches =
            bulk::build_edge_batches(edge_stream, &ordinal, budget).map_err(read_err)?;
        let total_edges = edge_batches.iter().map(|(n, _)| n).sum::<u64>();
        payload_bytes += edge_batches.iter().map(|(_, b)| b.len()).sum::<usize>();
        call_count += edge_batches.len();
        {
            let _permit = self.acquire_permit().await?;
            for (ec, blob) in &edge_batches {
                let mut cmd = redis::cmd("GRAPH.BULK");
                cmd.arg(&self.graph_key)
                    .arg(0)
                    .arg(*ec)
                    .arg(0)
                    .arg(1)
                    .arg(blob.as_slice());
                let _reply: Value = cmd
                    .query_async(&mut con)
                    .await
                    .map_err(|e| GraphStoreError::Backend(format!("GRAPH.BULK edges: {e}")))?;
            }
        }
        let load_ms = t_load.elapsed().as_millis();
        obs.edges_loaded(total_edges);

        // Indexes must be built after the bulk insert (see doc above).
        let t_idx = std::time::Instant::now();
        self.ensure_required_symbol_indexes().await?;
        obs.indexes_built();
        tracing::info!(
            nodes = total_nodes,
            edges = total_edges,
            payload_mb = payload_bytes / 1_048_576,
            calls = call_count,
            load_ms,
            index_ms = t_idx.elapsed().as_millis(),
            "falkor GRAPH.BULK load timings"
        );
        Ok(LoadStats {
            nodes: total_nodes,
            edges: total_edges,
        })
    }
}

// ---- helpers ----

fn duration_millis(duration: Duration) -> u64 {
    let millis = duration.as_millis().min(u128::from(u64::MAX)) as u64;
    if duration.is_zero() {
        0
    } else {
        millis.max(1)
    }
}

fn cell_has_token(cell: &str, expected: &str) -> bool {
    cell.split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .any(|token| token == expected)
}

async fn neighbor_nodes(
    store: &FalkorStore,
    id: &NodeId,
    dir: Direction,
    limit: usize,
    after: Option<&ContextCursorKey>,
) -> Result<ContextSection<Node>> {
    let arrow = match dir {
        Direction::Upstream => "<-[:CALLS]-",
        Direction::Downstream => "-[:CALLS]->",
        Direction::Both => "-[:CALLS]-",
    };
    let columns = node_columns("m");
    let (cursor_params, cursor_predicate) = after.map_or_else(
        || (String::new(), String::new()),
        |after| {
            (
                format!(
                    " after_name={} after_id={}",
                    cstr(&after.name),
                    cstr(&after.id)
                ),
                "WHERE m.name > $after_name OR (m.name = $after_name AND m.id > $after_id) "
                    .to_string(),
            )
        },
    );
    let probe_limit = limit + 1;
    let q = format!(
        "CYPHER id={id}{cursor_params} \
         MATCH (n:Symbol {{id:$id}}){arrow}(m:Symbol) \
         {cursor_predicate}\
         RETURN DISTINCT {columns} \
         ORDER BY m.name, m.id LIMIT {probe_limit}",
        id = cstr(id.as_str())
    );
    let nodes = store
        .rows(&q)
        .await?
        .iter()
        .map(|r| node_from_row(r))
        .collect();
    Ok(ContextSection::from_node_probe(nodes, limit, after))
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
