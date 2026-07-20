//! CIH MCP server library.
//!
//! The public surface is deliberately small: [`run`] (the server entry point
//! used by the `cih-server` binary) plus the modules exercised by integration
//! tests (`args`, `browser`, `patterns`, `search`, `utils`, `viz`, `wiki`).
//! Everything else is crate-private wiring.

mod app;
mod application;
mod startup;

pub(crate) mod app_error;
pub mod args;
pub mod browser;
pub mod patterns;
pub mod search;
pub mod utils;
pub mod viz;
pub mod wiki;

pub(crate) mod artifact_cache;
pub(crate) mod blocking;
pub(crate) mod config;
pub(crate) mod coverage;
pub(crate) mod feature;
pub(crate) mod files;
pub(crate) mod indexing;
pub(crate) mod jobs;
pub(crate) mod layout;
pub(crate) mod mtime_cache;
pub(crate) mod repo_context;
pub(crate) mod resources;
pub(crate) mod server;
pub(crate) mod single_flight;
pub(crate) mod symbol;
pub(crate) mod weighted_cache;
pub(crate) mod xflow;

pub use startup::run;
