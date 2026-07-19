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
    match backend {
        #[cfg(feature = "falkor")]
        "falkor" => {
            let mut store = cih_falkor::FalkorStore::connect(url, graph_key)
                .map_err(|e| anyhow::anyhow!("FalkorDB connect: {e}"))?;
            if let Some((max_concurrent, acquire_timeout)) = opts.query_limit {
                store = store.with_query_limit(max_concurrent, acquire_timeout);
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
