//! `cih-server` binary — thin shim over [`cih_server_lib::run`].
//!
//! All server logic (config, tool definitions, axum wiring) lives in the
//! library crate so it compiles once and is reachable from tests.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cih_server_lib::run().await
}
