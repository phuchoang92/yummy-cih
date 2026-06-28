//! Confidence scoring constants for all taint phases.
//!
//! Phase 0 baseline: `PHASE0_BASE − (edge_count − 1) × PHASE0_HOP_PENALTY`, floor `PHASE0_FLOOR`.
//! Phase 1 multiplier applied to Phase 0 baseline: `PHASE1_CONFIRMED` or `PHASE1_NO_FLOW`.
//! Phase 3 multiplier applied to Phase 0 baseline: `PHASE3_CONFIRMED`, `PHASE3_CONDITIONAL`, or `PHASE3_CLEAN`.

/// Phase 0: confidence for a direct (1-hop) source→sink path.
pub(crate) const PHASE0_BASE: f32 = 0.9;
/// Phase 0: confidence reduction per additional hop beyond the first.
pub(crate) const PHASE0_HOP_PENALTY: f32 = 0.05;
/// Phase 0: minimum confidence floor regardless of hop count.
pub(crate) const PHASE0_FLOOR: f32 = 0.5;

/// Phase 1: multiplier when intra-proc liveness confirms a tainted sink call.
pub(crate) const PHASE1_CONFIRMED: f32 = 1.15;
/// Phase 1: multiplier when intra-proc analysis finds no taint flow to a sink.
pub(crate) const PHASE1_NO_FLOW: f32 = 0.75;

/// Phase 3: multiplier when a tainted data-dep chain confirms a sink.
pub(crate) const PHASE3_CONFIRMED: f32 = 1.30;
/// Phase 3: multiplier when only a control-dep taint path is found.
pub(crate) const PHASE3_CONDITIONAL: f32 = 0.85;
/// Phase 3: multiplier when Phase 3 ran but found no taint evidence at all.
pub(crate) const PHASE3_CLEAN: f32 = 0.60;
