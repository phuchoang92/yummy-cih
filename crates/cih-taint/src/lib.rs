//! CIH taint analysis — Phase 0 inter-procedural + Phase 1–3 intra-procedural.
//!
//! # Quick-start
//!
//! The simplest entry point is `run_taint_analysis` in [`analyzer`], which runs all
//! enabled phases in sequence and returns scored `TaintPath` results.
//!
//! For fine-grained control, call the phase functions directly:
//!
//! ```rust,ignore
//! // Phase 0: inter-procedural BFS on the call graph
//! let paths = cih_taint::find_taint_paths(&nodes, &edges, &cih_taint::default_rules());
//!
//! // Phase 1: confirm paths by checking intra-procedural data flow (flow-insensitive)
//! let refinements = cih_taint::liveness::refine_paths(
//!     &paths,
//!     &|id| node_map.get(id).map(|n| n.file.clone()),
//!     |file| std::fs::read_to_string(repo.join(file)).ok(),
//!     &["execute", "exec", "write"],
//! );
//!
//! // Phase 3: PDG-based flow-sensitive kill-aware taint
//! let p3 = cih_taint::flow_sensitive::refine_paths(
//!     &paths,
//!     &|id| node_map.get(id).map(|n| n.file.clone()),
//!     |file| std::fs::read_to_string(repo.join(file)).ok(),
//!     &["execute", "exec"],
//!     &[],
//! );
//! ```
//!
//! # Architecture
//!
//! - **Phase 0** (`pass`): BFS on existing `CALLS` edges — method granularity, no new IR.
//! - **Phase 1** (`liveness` + `java_ir` + `ir`): flow-insensitive liveness taint.
//! - **Phase 2** (`cfg`): CFG construction + Cooper-Harvey-Kennedy dominance tree.
//! - **Phase 3** (`pdg` + `flow_sensitive`): reaching-definitions data-flow → PDG → flow-sensitive,
//!   kill-aware taint. Tracks which *definitions* are tainted (not just variables).
//!
//! See `docs/plans/cfg-pdg-taint-analysis.md` for the full design rationale.

pub(crate) mod confidence;
pub(crate) mod java_ast;

pub mod analyzer;
pub mod cfg;
pub mod error;
pub mod ir;
pub mod java_ir;
pub mod interproc;
pub mod pdg;
pub mod liveness;
pub(crate) mod queue;
pub mod rules;
pub mod flow_sensitive;

pub use analyzer::{
    run_taint_analysis, TaintAnalysisInput, TaintAnalysisResult, TaintCfgStats, TaintPdgStats,
    TaintPass, TaintPhaseConfig,
};
pub use cfg::{build_cfg, BasicBlock, BlockId, Cfg, CfgEdgeKind, DomTree};
pub use error::{TaintError, TaintResult};
pub use ir::{MethodBody, StatementKind, StatementNode};
pub use interproc::{find_taint_paths, TaintPath};
pub use pdg::{build_pdg, compute_reaching_defs, param_def_id, Pdg, PdgEdge, PdgEdgeKind, ReachingDefs};
pub use liveness::{analyze_method, ConfirmedSink, IntraResult, PathRefinement};
pub use rules::{default_rules, Language, SinkCategory, TaintRules, TaintSanitizer, TaintSink};
pub use flow_sensitive::{analyze_with_pdg, PdgRefinement, PdgResult, PdgSink};
