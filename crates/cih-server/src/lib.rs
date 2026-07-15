//! CIH MCP server library.
//!
//! The public surface is deliberately small: [`run`] (the server entry point
//! used by the `cih-server` binary) plus the modules exercised by integration
//! tests (`args`, `browser`, `patterns`, `search`, `utils`, `viz`, `wiki`).
//! Everything else is crate-private wiring.

mod app;
mod startup;

pub mod args;
pub mod browser;
pub mod patterns;
pub mod search;
pub mod utils;
pub mod viz;
pub mod wiki;

pub(crate) mod changes;
pub(crate) mod config;
pub(crate) mod contracts;
pub(crate) mod coverage;
pub(crate) mod feature;
pub(crate) mod files;
pub(crate) mod indexing;
pub(crate) mod jobs;
pub(crate) mod layout;
pub(crate) mod resources;
pub(crate) mod server;
pub(crate) mod symbol;
pub(crate) mod taint;
pub(crate) mod xflow;

pub use startup::run;
