//! `cih-engine` binary — thin shim over [`cih_engine::cmd::main`].
//!
//! The clap surface lives in `cmd/args.rs`; dispatch and per-command settings
//! resolution live in the `cmd` module, so the CLI layer compiles once and is
//! testable from the library crate.

fn main() -> anyhow::Result<()> {
    cih_engine::cmd::main()
}
