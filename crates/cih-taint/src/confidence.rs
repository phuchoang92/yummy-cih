//! Confidence scoring constants for all taint analysis passes.
//!
//! Inter-proc baseline: `INTERPROC_BASE − (edge_count − 1) × INTERPROC_HOP_PENALTY`, floor `INTERPROC_FLOOR`.
//! Liveness multiplier applied to inter-proc baseline: `LIVENESS_CONFIRMED` or `LIVENESS_NO_FLOW`.
//! PDG multiplier applied to inter-proc baseline: `PDG_CONFIRMED`, `PDG_CONDITIONAL`, or `PDG_CLEAN`.

/// Inter-procedural BFS: confidence for a direct (1-hop) source→sink path.
pub(crate) const INTERPROC_BASE: f32 = 0.9;
/// Inter-procedural BFS: confidence reduction per additional hop beyond the first.
pub(crate) const INTERPROC_HOP_PENALTY: f32 = 0.05;
/// Inter-procedural BFS: minimum confidence floor regardless of hop count.
pub(crate) const INTERPROC_FLOOR: f32 = 0.5;

/// Flow-insensitive liveness: multiplier when intra-proc analysis confirms a tainted sink call.
pub(crate) const LIVENESS_CONFIRMED: f32 = 1.15;
/// Flow-insensitive liveness: multiplier when intra-proc analysis finds no taint flow to a sink.
pub(crate) const LIVENESS_NO_FLOW: f32 = 0.75;

/// PDG flow-sensitive: multiplier when a tainted data-dep chain confirms a sink.
pub(crate) const PDG_CONFIRMED: f32 = 1.30;
/// PDG flow-sensitive: multiplier when only a control-dep taint path is found.
pub(crate) const PDG_CONDITIONAL: f32 = 0.85;
/// PDG flow-sensitive: multiplier when PDG analysis ran but found no taint evidence at all.
pub(crate) const PDG_CLEAN: f32 = 0.60;
