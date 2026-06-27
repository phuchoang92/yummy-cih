//! CIH taint analysis — Phase 0 inter-procedural + Phase 1–3 intra-procedural.
//!
//! # Quick-start
//!
//! ```rust,ignore
//! // Phase 0: inter-procedural BFS on the call graph
//! let paths = cih_taint::find_taint_paths(&nodes, &edges, &cih_taint::default_rules());
//!
//! // Phase 1: confirm paths by checking intra-procedural data flow (flow-insensitive)
//! let refinements = cih_taint::phase1::refine_paths(
//!     &paths,
//!     &|id| node_map.get(id).map(|n| n.file.clone()),
//!     |file| std::fs::read_to_string(repo.join(file)).ok(),
//!     &["execute", "exec", "write"],
//! );
//!
//! // Phase 3: PDG-based flow-sensitive kill-aware taint
//! let p3 = cih_taint::taint3::refine_paths_phase3(
//!     &paths,
//!     &|id| node_map.get(id).map(|n| n.file.clone()),
//!     |file| std::fs::read_to_string(repo.join(file)).ok(),
//!     |_id| vec!["input".to_string()],
//!     &["execute", "exec"],
//! );
//! ```
//!
//! # Architecture
//!
//! - **Phase 0** (`pass`): BFS on existing `CALLS` edges — method granularity, no new IR.
//! - **Phase 1** (`phase1` + `java_ir` + `ir`): flow-insensitive liveness taint.
//! - **Phase 2** (`cfg`): CFG construction + Cooper-Harvey-Kennedy dominance tree.
//! - **Phase 3** (`pdg` + `taint3`): reaching-definitions data-flow → PDG → flow-sensitive,
//!   kill-aware taint. Tracks which *definitions* are tainted (not just variables).
//!
//! Demand-driven triggering is coordinated via [`queue::CfgRequestQueue`].
//! See `docs/plans/cfg-pdg-taint-analysis.md` for the full roadmap.

pub mod cfg;
pub mod ir;
pub mod java_ir;
pub mod pass;
pub mod pdg;
pub mod phase1;
pub mod queue;
pub mod rules;
pub mod taint3;

pub use cfg::{build_cfg, BasicBlock, BlockId, Cfg, CfgEdgeKind, DomTree};
pub use ir::{MethodBody, StatementKind, StatementNode};
pub use java_ir::extract_method_body;
pub use pass::{find_taint_paths, TaintPath};
pub use pdg::{build_pdg, compute_reaching_defs, param_def_id, Pdg, PdgEdge, PdgEdgeKind, ReachingDefs};
pub use phase1::{analyze_method, ConfirmedSink, IntraResult, PathRefinement};
pub use queue::{CfgRequest, CfgRequestQueue, CfgTrigger};
pub use rules::{default_rules, SinkCategory, TaintRules, TaintSanitizer, TaintSink};
pub use taint3::{analyze_with_pdg, ConfirmedSink3, Phase3Refinement, Phase3Result};
