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

pub mod analyze;
pub mod cmd;
pub mod db;
pub mod discover;
pub mod file_cache;
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
