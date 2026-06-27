//! CIH taint analysis — Phase 0 (inter-procedural, method-granularity).
//!
//! # Usage
//!
//! ```rust,ignore
//! let nodes = artifacts.read_nodes()?;
//! let edges = artifacts.read_edges()?;
//! let paths = cih_taint::find_taint_paths(&nodes, &edges, &cih_taint::default_rules());
//! for path in &paths {
//!     println!("{} → {} ({:?})", path.source, path.sink_method, path.category);
//!     let edge = path.to_edge(); // TaintFlow edge ready for the graph store
//! }
//! ```
//!
//! # Architecture
//!
//! Phase 0 runs entirely on the existing method-granularity call graph — no new IR.
//! Phases 1–3 will add on-demand statement-level CFG/PDG via [`queue::CfgRequestQueue`].
//! See `docs/plans/cfg-pdg-taint-analysis.md` for the full roadmap.

pub mod pass;
pub mod queue;
pub mod rules;

pub use pass::{find_taint_paths, TaintPath};
pub use queue::{CfgRequest, CfgRequestQueue, CfgTrigger};
pub use rules::{default_rules, SinkCategory, TaintRules, TaintSanitizer, TaintSink};
