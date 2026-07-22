//! Runtime configuration and validated environment policy.
//!
//! `CIH_GRAPH_BACKEND` selects the adapter — `falkor` now (dev / open-source),
//! `neptune` at go-live, `postgres` as the ~$0 fallback. Swapping backends is a
//! one-line env change; nothing else in the server cares which store it is.

use std::path::PathBuf;

use anyhow::{anyhow, Result};

#[derive(Clone)]
pub struct Config {
    pub backend: String,
    pub bind: String,
    pub falkor_url: String,
    pub graph_key: String,
    /// Optional home group (`CIH_GROUP`). When set, `list_repos` scopes to the
    /// group's members — the multi-repo serving mode where one server fronts a
    /// whole microservice and per-tool `repo` selects the member.
    pub group: Option<String>,
    pub artifacts_dir: Option<PathBuf>,
    pub pg_url: Option<String>,
    /// Optional static bearer token to protect /mcp and /graph. Unset = open (dev mode).
    pub api_token: Option<String>,
    /// Escape hatch: allow a non-loopback bind without an API token (trusted network).
    pub allow_insecure: bool,
    /// Max bytes `read_file` will load from a single file before erroring.
    pub read_file_max_bytes: u64,
    /// Max lines `read_file` returns when no explicit line range is given.
    pub read_file_max_lines: usize,
    /// Max concurrent Cypher queries against the graph store (backpressure). Set
    /// near the FalkorDB `THREAD_COUNT` (default = cores) for best throughput.
    pub max_concurrent_queries: usize,
    /// Max wait (ms) for a query slot before shedding with an "overloaded" error.
    pub query_queue_timeout_ms: u64,
}

/// Default `read_file` byte cap (10 MiB).
pub const DEFAULT_READ_FILE_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Default `read_file` line cap when no range is requested.
pub const DEFAULT_READ_FILE_MAX_LINES: usize = 5000;
/// Default cap on concurrent Cypher queries against the graph store.
pub const DEFAULT_MAX_CONCURRENT_QUERIES: usize = 64;
/// Default max wait (ms) for a query slot before shedding.
pub const DEFAULT_QUERY_QUEUE_TIMEOUT_MS: u64 = 5000;
pub const DEFAULT_ARTIFACT_CACHE_MAX_BYTES: usize = 512 * 1024 * 1024;
pub const DEFAULT_WIKI_CACHE_MAX_BYTES: usize = 256 * 1024 * 1024;
pub const DEFAULT_SEARCH_CACHE_MAX_BYTES: usize = 256 * 1024 * 1024;
/// Default budget for JSONL resource paging indexes (8 bytes per matching
/// record — two orders of magnitude smaller than the snapshot caches).
pub const DEFAULT_RESOURCE_INDEX_CACHE_MAX_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_TOTAL_CACHE_MAX_BYTES: usize = 1040 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RetrievalConfig {
    pub(crate) search_cache_max_entries: usize,
    pub(crate) search_score_max_concurrent: usize,
    pub(crate) search_score_queue_timeout_ms: u64,
    pub(crate) search_cold_max_concurrent: usize,
    pub(crate) search_cold_max_bytes: usize,
    pub(crate) search_cold_queue_timeout_secs: u64,
    pub(crate) search_sidecar_enabled: bool,
    pub(crate) grep_max_concurrent_requests: usize,
    pub(crate) grep_threads: usize,
    pub(crate) grep_queue_timeout_secs: u64,
    pub(crate) grep_deadline_secs: u64,
    pub(crate) wiki_live_max_nodes: usize,
}

impl RetrievalConfig {
    pub(crate) fn from_env() -> Result<Self> {
        let cpus = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        let config = Self {
            search_cache_max_entries: positive_env("CIH_SEARCH_CACHE_MAX_ENTRIES", 32usize)?,
            search_score_max_concurrent: positive_env(
                "CIH_SEARCH_SCORE_MAX_CONCURRENT",
                cpus.min(4).max(1),
            )?,
            search_score_queue_timeout_ms: positive_env(
                "CIH_SEARCH_SCORE_QUEUE_TIMEOUT_MS",
                2_000u64,
            )?,
            search_cold_max_concurrent: positive_env("CIH_SEARCH_COLD_MAX_CONCURRENT", 1usize)?,
            search_cold_max_bytes: positive_env(
                "CIH_SEARCH_COLD_MAX_BYTES",
                512usize * 1024 * 1024,
            )?,
            search_cold_queue_timeout_secs: positive_env(
                "CIH_SEARCH_COLD_QUEUE_TIMEOUT_SECS",
                5u64,
            )?,
            search_sidecar_enabled: bool_env("CIH_SEARCH_SIDECAR_ENABLED", true)?,
            grep_max_concurrent_requests: positive_env("CIH_GREP_MAX_CONCURRENT_REQUESTS", 1usize)?,
            grep_threads: positive_env("CIH_GREP_THREADS", cpus.min(4).max(1))?,
            grep_queue_timeout_secs: positive_env("CIH_GREP_QUEUE_TIMEOUT_SECS", 2u64)?,
            grep_deadline_secs: positive_env("CIH_GREP_DEADLINE_SECS", 80u64)?,
            wiki_live_max_nodes: positive_env("CIH_WIKI_LIVE_MAX_NODES", 100_000usize)?,
        };
        let blocking_timeout_secs = match std::env::var("CIH_BLOCKING_TIMEOUT_SECS") {
            Ok(raw) => raw.parse::<u64>().map_err(|_| {
                anyhow!("CIH_BLOCKING_TIMEOUT_SECS must be a non-negative integer (got '{raw}')")
            })?,
            Err(std::env::VarError::NotPresent) => 90,
            Err(error) => return Err(anyhow!("cannot read CIH_BLOCKING_TIMEOUT_SECS: {error}")),
        };
        validate_grep_deadlines(
            config.grep_queue_timeout_secs,
            config.grep_deadline_secs,
            blocking_timeout_secs,
        )?;
        Ok(config)
    }
}

fn positive_env<T>(name: &'static str, default: T) -> Result<T>
where
    T: std::str::FromStr + PartialEq + Default + Copy,
{
    let value = match std::env::var(name) {
        Ok(raw) => raw
            .parse::<T>()
            .map_err(|_| anyhow!("{name} must be a positive integer (got '{raw}')"))?,
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => return Err(anyhow!("cannot read {name}: {error}")),
    };
    if value == T::default() {
        return Err(anyhow!("{name} must be greater than zero"));
    }
    Ok(value)
}

fn bool_env(name: &'static str, default: bool) -> Result<bool> {
    match std::env::var(name) {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(anyhow!(
                "{name} must be one of true/false, 1/0, yes/no, or on/off (got '{raw}')"
            )),
        },
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(anyhow!("cannot read {name}: {error}")),
    }
}

fn validate_grep_deadlines(queue: u64, deadline: u64, blocking: u64) -> Result<()> {
    if blocking == 0 {
        return Ok(());
    }
    let required = queue
        .checked_add(deadline)
        .and_then(|value| value.checked_add(5))
        .ok_or_else(|| anyhow!("grep queue/deadline configuration overflows u64"))?;
    if required > blocking {
        return Err(anyhow!(
            "CIH_GREP_QUEUE_TIMEOUT_SECS + CIH_GREP_DEADLINE_SECS must leave at least 5 seconds before CIH_BLOCKING_TIMEOUT_SECS ({queue} + {deadline} + 5 > {blocking})"
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CacheBudgets {
    pub(crate) artifact_bytes: usize,
    pub(crate) wiki_bytes: usize,
    pub(crate) search_bytes: usize,
    pub(crate) resource_index_bytes: usize,
    pub(crate) total_bytes: usize,
}

impl CacheBudgets {
    pub(crate) fn from_env() -> Result<Self> {
        Self::parse(
            std::env::var("CIH_ARTIFACT_CACHE_MAX_BYTES").ok(),
            std::env::var("CIH_WIKI_CACHE_MAX_BYTES").ok(),
            std::env::var("CIH_SEARCH_CACHE_MAX_BYTES").ok(),
            std::env::var("CIH_RESOURCE_INDEX_CACHE_MAX_BYTES").ok(),
            std::env::var("CIH_CACHE_MAX_BYTES").ok(),
        )
    }

    fn parse(
        artifact: Option<String>,
        wiki: Option<String>,
        search: Option<String>,
        resource_index: Option<String>,
        total: Option<String>,
    ) -> Result<Self> {
        let parse = |name: &'static str, value: Option<String>, default: usize| {
            let value = match value {
                Some(raw) => raw.parse::<usize>().map_err(|_| {
                    anyhow!("{name} must be a positive integer byte count (got '{raw}')")
                })?,
                None => default,
            };
            if value == 0 {
                return Err(anyhow!("{name} must be greater than zero"));
            }
            Ok(value)
        };
        let budgets = Self {
            artifact_bytes: parse(
                "CIH_ARTIFACT_CACHE_MAX_BYTES",
                artifact,
                DEFAULT_ARTIFACT_CACHE_MAX_BYTES,
            )?,
            wiki_bytes: parse(
                "CIH_WIKI_CACHE_MAX_BYTES",
                wiki,
                DEFAULT_WIKI_CACHE_MAX_BYTES,
            )?,
            search_bytes: parse(
                "CIH_SEARCH_CACHE_MAX_BYTES",
                search,
                DEFAULT_SEARCH_CACHE_MAX_BYTES,
            )?,
            resource_index_bytes: parse(
                "CIH_RESOURCE_INDEX_CACHE_MAX_BYTES",
                resource_index,
                DEFAULT_RESOURCE_INDEX_CACHE_MAX_BYTES,
            )?,
            total_bytes: parse("CIH_CACHE_MAX_BYTES", total, DEFAULT_TOTAL_CACHE_MAX_BYTES)?,
        };
        let configured = budgets
            .artifact_bytes
            .checked_add(budgets.wiki_bytes)
            .and_then(|sum| sum.checked_add(budgets.search_bytes))
            .and_then(|sum| sum.checked_add(budgets.resource_index_bytes))
            .ok_or_else(|| anyhow!("configured cache budgets overflow usize"))?;
        if configured > budgets.total_bytes {
            return Err(anyhow!(
                "configured cache budgets total {configured} bytes, exceeding \
                 CIH_CACHE_MAX_BYTES={} (artifact={}, wiki={}, search={}, resource_index={})",
                budgets.total_bytes,
                budgets.artifact_bytes,
                budgets.wiki_bytes,
                budgets.search_bytes,
                budgets.resource_index_bytes
            ));
        }
        Ok(budgets)
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("backend", &self.backend)
            .field("bind", &self.bind)
            .field("falkor_url", &self.falkor_url)
            .field("graph_key", &self.graph_key)
            .field("group", &self.group)
            .field("artifacts_dir", &self.artifacts_dir)
            .field("pg_url", &self.pg_url.as_deref().map(|_| "[set]"))
            .field(
                "api_token",
                &self.api_token.as_deref().map(|_| "[REDACTED]"),
            )
            .field("allow_insecure", &self.allow_insecure)
            .field("max_concurrent_queries", &self.max_concurrent_queries)
            .field("query_queue_timeout_ms", &self.query_queue_timeout_ms)
            .finish()
    }
}

impl Config {
    pub fn from_env() -> Self {
        let backend = env("CIH_GRAPH_BACKEND", "falkor");
        // Shared default (factory crate): redis://…:6380 for falkor — the
        // documented local port (Homebrew redis squats 6379) — or the
        // ~/.cih/ladybug filesystem root for the embedded backend. Deployments
        // set FALKOR_URL explicitly, so this only affects bare local runs.
        let default_url = cih_store_factory::default_url(&backend);
        Self {
            backend,
            bind: env("CIH_BIND", "127.0.0.1:8080"),
            falkor_url: env("FALKOR_URL", &default_url),
            graph_key: env("CIH_GRAPH_KEY", "cih"),
            group: std::env::var("CIH_GROUP").ok().filter(|s| !s.is_empty()),
            artifacts_dir: std::env::var("CIH_ARTIFACTS_DIR").ok().map(PathBuf::from),
            pg_url: std::env::var("CIH_PG_URL").ok(),
            api_token: std::env::var("CIH_API_TOKEN").ok(),
            allow_insecure: std::env::var("CIH_ALLOW_INSECURE")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            read_file_max_bytes: env_parse("CIH_READ_FILE_MAX_BYTES", DEFAULT_READ_FILE_MAX_BYTES),
            read_file_max_lines: env_parse("CIH_READ_FILE_MAX_LINES", DEFAULT_READ_FILE_MAX_LINES),
            max_concurrent_queries: env_parse(
                "CIH_MAX_CONCURRENT_QUERIES",
                DEFAULT_MAX_CONCURRENT_QUERIES,
            ),
            query_queue_timeout_ms: env_parse(
                "CIH_QUERY_QUEUE_TIMEOUT_MS",
                DEFAULT_QUERY_QUEUE_TIMEOUT_MS,
            ),
        }
    }

    /// Enforce that a network-exposed server has authentication.
    ///
    /// Returns an error when the bind address is non-loopback and no
    /// `CIH_API_TOKEN` is set, unless `CIH_ALLOW_INSECURE` opts out. Loopback
    /// binds stay open (dev mode) with only a warning at the call site.
    pub fn check_auth_posture(&self) -> Result<()> {
        if self.api_token.is_some() || self.allow_insecure || bind_is_loopback(&self.bind) {
            return Ok(());
        }
        Err(anyhow!(
            "refusing to start: CIH_BIND='{}' is network-exposed but CIH_API_TOKEN is not set. \
             Set CIH_API_TOKEN=<secret> to require a bearer token, or set CIH_ALLOW_INSECURE=1 \
             to run open on a trusted network.",
            self.bind
        ))
    }
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Parse an env var into `T`, falling back to `default` when unset or invalid.
fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// True when `bind` (a `host:port` string) targets a loopback interface.
///
/// A missing/unparseable host is treated as non-loopback so the auth check
/// fails safe. `0.0.0.0` and `::` (all-interfaces) are explicitly non-loopback.
fn bind_is_loopback(bind: &str) -> bool {
    let host = match bind.rsplit_once(':') {
        Some((h, _)) => h.trim_matches(['[', ']']),
        None => bind.trim_matches(['[', ']']),
    };
    if host == "localhost" {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

/// The `StoreOptions` this server applies to every store it constructs
/// (startup primary + per-graph-key stores), so tuning is uniform.
pub fn store_options(cfg: &Config) -> cih_store_factory::StoreOptions {
    cih_store_factory::StoreOptions {
        query_limit: Some((
            cfg.max_concurrent_queries,
            std::time::Duration::from_millis(cfg.query_queue_timeout_ms),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bind_is_loopback, validate_grep_deadlines, CacheBudgets, DEFAULT_ARTIFACT_CACHE_MAX_BYTES,
        DEFAULT_RESOURCE_INDEX_CACHE_MAX_BYTES, DEFAULT_SEARCH_CACHE_MAX_BYTES,
        DEFAULT_WIKI_CACHE_MAX_BYTES,
    };

    #[test]
    fn grep_deadline_must_fit_inside_blocking_timeout() {
        assert!(validate_grep_deadlines(2, 80, 90).is_ok());
        assert!(validate_grep_deadlines(5, 85, 90).is_err());
        assert!(validate_grep_deadlines(5, 85, 0).is_ok());
    }

    #[test]
    fn loopback_binds_are_recognized() {
        assert!(bind_is_loopback("127.0.0.1:8080"));
        assert!(bind_is_loopback("localhost:8080"));
        assert!(bind_is_loopback("[::1]:8080"));
        assert!(bind_is_loopback("127.0.0.1"));
    }

    #[test]
    fn exposed_binds_are_not_loopback() {
        assert!(!bind_is_loopback("0.0.0.0:8080"));
        assert!(!bind_is_loopback("[::]:8080"));
        assert!(!bind_is_loopback("192.168.1.10:8080"));
        assert!(!bind_is_loopback("10.0.0.5:8080"));
        // Unparseable host fails safe (treated as exposed).
        assert!(!bind_is_loopback("not-an-addr:8080"));
    }

    #[test]
    fn cache_budgets_reject_invalid_or_excessive_totals() {
        assert!(CacheBudgets::parse(Some("0".into()), None, None, None, None).is_err());
        let error = CacheBudgets::parse(
            Some("8".into()),
            Some("8".into()),
            Some("8".into()),
            Some("2".into()),
            Some("16".into()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("exceeding CIH_CACHE_MAX_BYTES"));
        // The paging-index budget counts toward the validated total: the same
        // partition fits under 17 but overcommits 16.
        assert!(CacheBudgets::parse(
            Some("8".into()),
            Some("4".into()),
            Some("4".into()),
            Some("1".into()),
            Some("17".into()),
        )
        .is_ok());
        assert!(CacheBudgets::parse(
            Some("8".into()),
            Some("4".into()),
            Some("4".into()),
            Some("1".into()),
            Some("16".into()),
        )
        .is_err());
    }

    #[test]
    fn cache_budgets_accept_a_bounded_partition() {
        let budgets = CacheBudgets::parse(
            Some("8".into()),
            Some("4".into()),
            Some("4".into()),
            Some("1".into()),
            Some("17".into()),
        )
        .unwrap();
        assert_eq!(budgets.artifact_bytes, 8);
        assert_eq!(budgets.wiki_bytes, 4);
        assert_eq!(budgets.search_bytes, 4);
        assert_eq!(budgets.resource_index_bytes, 1);
        assert_eq!(budgets.total_bytes, 17);
    }

    /// The shipped defaults must satisfy their own validation.
    #[test]
    fn default_cache_budgets_fit_the_default_total() {
        let budgets = CacheBudgets::parse(None, None, None, None, None).unwrap();
        assert_eq!(
            budgets.artifact_bytes
                + budgets.wiki_bytes
                + budgets.search_bytes
                + budgets.resource_index_bytes,
            DEFAULT_ARTIFACT_CACHE_MAX_BYTES
                + DEFAULT_WIKI_CACHE_MAX_BYTES
                + DEFAULT_SEARCH_CACHE_MAX_BYTES
                + DEFAULT_RESOURCE_INDEX_CACHE_MAX_BYTES
        );
        assert!(
            budgets.artifact_bytes
                + budgets.wiki_bytes
                + budgets.search_bytes
                + budgets.resource_index_bytes
                <= budgets.total_bytes
        );
    }
}
