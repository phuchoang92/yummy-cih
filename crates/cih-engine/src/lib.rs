//! CIH engine library: the `scan → parse → resolve → load → discover → embed
//! → wiki` pipeline behind the `cih-engine` binary (a thin shim over
//! [`cmd::main`]).
//!
//! Organization:
//! - [`cmd`] — the CLI layer: clap surface, dispatch, per-command settings
//!   resolution, and every command implementation.
//! - [`analyze`], [`scan`], [`discover`], `embed`, `decompile` — pipeline
//!   phases (analyze orchestrates parse/resolve/emit).
//! - [`wiki`] + [`llm`] — LLM enrichment and provider adapters (rendering
//!   lives in the `cih-wiki` crate).
//! - The rest are shared utilities: [`db`] (FalkorDB staging/publish),
//!   [`settings`] (layered config), [`scope`], [`file_cache`],
//!   [`versioning`], `registry`, `runtime`, `ui`, `feature_strategy`.
//!
//! `pub` modules are the surface the integration tests exercise; everything
//! else is `pub(crate)`.

#[doc(hidden)]
pub const DEFAULT_FALKOR_URL: &str = "redis://127.0.0.1:6380";
#[doc(hidden)]
pub const DEFAULT_GRAPH_KEY: &str = "cih";

/// Resolve the Postgres URL: an explicit flag value wins, else `$CIH_PG_URL`.
/// Returns `None` when neither is set; callers add their own context message.
pub fn resolve_pg_url(explicit: Option<String>) -> Option<String> {
    explicit.or_else(|| std::env::var("CIH_PG_URL").ok())
}

#[cfg(test)]
mod config_tests {
    #[test]
    fn resolve_pg_url_prefers_explicit_over_env() {
        // Explicit Some short-circuits before the env lookup, so this is
        // deterministic regardless of CIH_PG_URL in the environment.
        assert_eq!(
            super::resolve_pg_url(Some("postgres://explicit".into())).as_deref(),
            Some("postgres://explicit"),
        );
    }
}

pub mod analyze;
pub mod cmd;
pub mod db;
pub mod discover;
pub mod file_cache;
pub mod group_sync;
pub mod llm;
pub mod scan;
pub mod scope;
pub mod settings;
pub mod versioning;
pub mod wiki;

pub(crate) mod decompile;
pub(crate) mod decompile_config;
pub(crate) mod embed;
pub(crate) mod feature_strategy;
pub(crate) mod node_prefix;
pub(crate) mod registry;
pub(crate) mod runtime;
pub(crate) mod ui;
