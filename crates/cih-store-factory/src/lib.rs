//! The single place graph-store adapters are named. Every consumer (engine
//! load path, server startup, per-graph-key stores) constructs stores through
//! [`connect_store`]; adapter crates are dependencies of this crate only,
//! behind Cargo features, so the rest of the workspace depends purely on the
//! `cih-graph-store` port.
//!
//! Adding a backend: implement `GraphStore` in a new crate, add a feature +
//! match arm here, and run the contract suite (see `docs/ARCHITECTURE.md`).

use std::sync::Arc;
use std::time::Duration;

use cih_graph_store::GraphStore;

/// Construction-time tuning that only some consumers apply.
#[derive(Clone, Debug, Default)]
pub struct StoreOptions {
    /// `(max_concurrent, acquire_timeout)` — server-side query backpressure.
    /// `None` for CLI use (loads are sequential; no limit needed).
    pub query_limit: Option<(usize, Duration)>,
}

/// Read-query deadlines kept separate from the legacy construction options so
/// existing `StoreOptions` struct literals remain source compatible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadTimeoutOptions {
    /// Backend-side execution deadline (FalkorDB `GRAPH.QUERY TIMEOUT`).
    pub backend: Duration,
    /// Driver-side deadline; must be longer than `backend`.
    pub driver: Duration,
}

/// Additive adapter runtime tuning. Unsupported adapters ignore an option
/// rather than making a backend switch fail at construction time.
#[derive(Clone, Debug, Default)]
pub struct StoreRuntimeOptions {
    pub read_timeouts: Option<ReadTimeoutOptions>,
    /// Optional compatibility wait after a backend reports dataset loading.
    /// Servers with a readiness monitor normally set this to zero so requests
    /// receive typed retry guidance promptly.
    pub read_loading_wait: Option<Duration>,
}

/// Default DB url for a backend when no explicit url/env is given. THE single
/// source of truth — engine and server both delegate here (they previously
/// disagreed: server said 6379, engine and the docs said 6380; FalkorDB runs
/// on 6380 locally because Homebrew redis squats 6379).
pub fn default_url(backend: &str) -> String {
    match backend {
        // Embedded backend: the "url" is a filesystem root.
        "ladybug" => cih_core::CihPaths::discover()
            .map(|paths| paths.graphs().to_string_lossy().into_owned())
            .unwrap_or_else(|| ".cih-ladybug".to_string()),
        _ => "redis://127.0.0.1:6380".to_string(),
    }
}

/// Backend names that are compiled into this build, for error messages.
pub fn compiled_backends() -> Vec<&'static str> {
    vec![
        #[cfg(feature = "falkor")]
        "falkor",
        #[cfg(feature = "ladybug")]
        "ladybug",
    ]
}

/// Connect a store for `backend` ("falkor" | "neptune" | "postgres"), without
/// touching the network — adapters connect lazily on first query. `url` is
/// backend-specific (Falkor: a redis:// URL). Unknown or not-compiled-in
/// backends error, listing the compiled-in ones.
pub fn connect_store(
    backend: &str,
    url: &str,
    graph_key: &str,
    opts: &StoreOptions,
) -> anyhow::Result<Arc<dyn GraphStore>> {
    connect_store_with_runtime(
        backend,
        url,
        graph_key,
        opts,
        &StoreRuntimeOptions::default(),
    )
}

/// Connect a store with optional runtime deadlines. This is additive to
/// [`connect_store`] so existing engine/server callers retain their behavior
/// until they explicitly inject bounded read settings.
pub fn connect_store_with_runtime(
    backend: &str,
    url: &str,
    graph_key: &str,
    opts: &StoreOptions,
    runtime: &StoreRuntimeOptions,
) -> anyhow::Result<Arc<dyn GraphStore>> {
    #[cfg(not(feature = "falkor"))]
    let _ = runtime;
    #[cfg(not(any(feature = "falkor", feature = "ladybug")))]
    let _ = (url, graph_key, opts);
    match backend {
        #[cfg(feature = "falkor")]
        "falkor" => {
            let mut store = cih_falkor::FalkorStore::connect(url, graph_key)
                .map_err(|e| anyhow::anyhow!("FalkorDB connect: {e}"))?;
            if let Some((max_concurrent, acquire_timeout)) = opts.query_limit {
                store = store.with_query_limit(max_concurrent, acquire_timeout);
            }
            if let Some(timeouts) = runtime.read_timeouts {
                store = store
                    .with_read_timeouts(timeouts.backend, timeouts.driver)
                    .map_err(|e| anyhow::anyhow!("FalkorDB read timeouts: {e}"))?;
            }
            if let Some(wait) = runtime.read_loading_wait {
                store = store.with_read_load_wait(wait);
            }
            Ok(Arc::new(store))
        }
        #[cfg(feature = "ladybug")]
        "ladybug" => {
            let mut store = cih_ladybug::LadybugStore::connect(url, graph_key)
                .map_err(|e| anyhow::anyhow!("LadybugDB open: {e}"))?;
            if let Some((max_concurrent, acquire_timeout)) = opts.query_limit {
                store = store.with_query_limit(max_concurrent, acquire_timeout);
            }
            Ok(Arc::new(store))
        }
        "neptune" => {
            anyhow::bail!("neptune adapter not implemented yet — go-live target (cih-neptune)")
        }
        "postgres" => {
            anyhow::bail!("postgres-cte adapter not implemented yet — ~$0 fallback (cih-postgres)")
        }
        other => anyhow::bail!(
            "unknown CIH_GRAPH_BACKEND='{other}' (compiled-in backends: {})",
            compiled_backends().join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_backend_lists_compiled_in_ones() {
        let Err(err) = connect_store("nosuch", "redis://x", "k", &StoreOptions::default()) else {
            panic!("unknown backend must error");
        };
        let msg = err.to_string();
        assert!(msg.contains("nosuch"), "names the bad backend: {msg}");
        #[cfg(feature = "falkor")]
        assert!(msg.contains("falkor"), "lists compiled-in backends: {msg}");
    }

    #[cfg(feature = "falkor")]
    #[test]
    fn falkor_arm_constructs_lazily_without_a_live_db() {
        // FalkorStore::connect is lazy (connects on first query), so this must
        // succeed with no DB running — the property hermetic tests rely on.
        connect_store(
            "falkor",
            "redis://127.0.0.1:6380",
            "factory_test",
            &StoreOptions {
                query_limit: Some((4, Duration::from_millis(100))),
            },
        )
        .expect("lazy construction");
    }

    #[cfg(feature = "falkor")]
    #[test]
    fn falkor_read_timeouts_are_validated_without_a_live_db() {
        let opts = StoreOptions::default();
        let valid = StoreRuntimeOptions {
            read_timeouts: Some(ReadTimeoutOptions {
                backend: Duration::from_secs(2),
                driver: Duration::from_secs(3),
            }),
            ..StoreRuntimeOptions::default()
        };
        connect_store_with_runtime(
            "falkor",
            "redis://127.0.0.1:6380",
            "factory_test",
            &opts,
            &valid,
        )
        .expect("valid read deadlines construct lazily");

        for invalid in [
            ReadTimeoutOptions {
                backend: Duration::ZERO,
                driver: Duration::from_secs(1),
            },
            ReadTimeoutOptions {
                backend: Duration::from_secs(2),
                driver: Duration::from_secs(2),
            },
        ] {
            let result = connect_store_with_runtime(
                "falkor",
                "redis://127.0.0.1:6380",
                "factory_test",
                &opts,
                &StoreRuntimeOptions {
                    read_timeouts: Some(invalid),
                    ..StoreRuntimeOptions::default()
                },
            );
            let Err(error) = result else {
                panic!("invalid read deadlines must fail before dialing");
            };
            assert!(
                error.to_string().contains("timeout"),
                "error names invalid timeout: {error}"
            );
        }
    }

    #[test]
    fn stub_backends_error_distinctly() {
        for name in ["neptune", "postgres"] {
            let Err(err) = connect_store(name, "", "k", &StoreOptions::default()) else {
                panic!("stub backend {name} must error");
            };
            assert!(err.to_string().contains("not implemented"), "{name}");
        }
    }
}
