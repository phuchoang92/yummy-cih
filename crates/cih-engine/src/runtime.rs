use std::sync::OnceLock;

use anyhow::{Context, Result};

static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Return the process-wide single-threaded Tokio runtime, creating it on first call.
///
/// All async I/O in the CLI (FalkorDB loads, embedding, server) runs on this
/// one runtime. Creating multiple runtimes per command was wasteful and caused
/// confusing nested-runtime panics when commands were composed.
pub fn get() -> &'static tokio::runtime::Runtime {
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build Tokio runtime")
    })
}

/// Convenience: run a future to completion on the shared runtime.
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    get().block_on(f)
}

/// Build the shared runtime eagerly and return an error instead of panicking.
/// Call this from `main` so startup failures are user-visible.
pub fn init() -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build Tokio runtime")
        .map(|rt| {
            let _ = RT.set(rt);
        })
}
