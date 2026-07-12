//! Runtime config + the GraphStore factory (ports & adapters wiring).
//!
//! `CIH_GRAPH_BACKEND` selects the adapter — `falkor` now (dev / open-source),
//! `neptune` at go-live, `postgres` as the ~$0 fallback. Swapping backends is a
//! one-line env change; nothing else in the server cares which store it is.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use cih_graph_store::GraphStore;

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
        Self {
            backend: env("CIH_GRAPH_BACKEND", "falkor"),
            bind: env("CIH_BIND", "127.0.0.1:8080"),
            falkor_url: env("FALKOR_URL", "redis://127.0.0.1:6379"),
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

/// Build the configured `GraphStore`. This is the single place adapters are
/// named — the rest of the engine/MCP layer depends only on `dyn GraphStore`.
pub async fn build_store(cfg: &Config) -> Result<Arc<dyn GraphStore>> {
    match cfg.backend.as_str() {
        "falkor" => {
            let store = cih_falkor::FalkorStore::connect(&cfg.falkor_url, &cfg.graph_key)?
                .with_query_limit(
                    cfg.max_concurrent_queries,
                    std::time::Duration::from_millis(cfg.query_queue_timeout_ms),
                );
            let mut last_err = None;
            for attempt in 1u32..=5 {
                match store.ensure_schema().await {
                    Ok(_) => {
                        last_err = None;
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(attempt, error = %e, "FalkorDB not ready, retrying in 2s");
                        last_err = Some(e);
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                }
            }
            if let Some(e) = last_err {
                return Err(anyhow::anyhow!("FalkorDB schema init failed: {e}"));
            }
            Ok(Arc::new(store))
        }
        "neptune" => Err(anyhow!(
            "neptune adapter not implemented yet — go-live target (cih-neptune)"
        )),
        "postgres" => Err(anyhow!(
            "postgres-cte adapter not implemented yet — ~$0 fallback (cih-postgres)"
        )),
        other => Err(anyhow!(
            "unknown CIH_GRAPH_BACKEND='{other}' (use falkor|neptune|postgres)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::bind_is_loopback;

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
}
