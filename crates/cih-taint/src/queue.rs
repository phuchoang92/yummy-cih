//! Demand-driven CFG/PDG request queue (Phase 1+ stub).
//!
//! Phase 0 taint runs entirely on the method-granularity call graph. This module
//! provides the `CfgRequestQueue` that Phases 1–3 will use to trigger on-demand
//! intra-procedural analysis for specific methods.
//!
//! Trigger conditions (Phase 1+):
//!  1. The method is an API entry point (already tagged via `HandlesRoute` / `ListensTo`).
//!  2. The method appears on a Phase 0 taint path.
//!  3. An external caller (MCP tool, chat query) requests `analyze_method(fqn)`.
//!
//! Results are cached by `(fqn, ast_hash)` and evicted when the method body changes.
//! Statement IR and CFG/PDG graphs are never persisted to the main graph store —
//! they live only in memory for the duration of the analysis request.

use cih_core::NodeId;

/// A request to build an on-demand CFG/PDG for a specific method.
#[derive(Clone, Debug)]
pub struct CfgRequest {
    /// Fully-qualified method node ID (e.g. `Method:com.example.OrderService#save/1`).
    pub method_id: NodeId,
    /// Reason the request was enqueued.
    pub trigger: CfgTrigger,
}

/// What triggered this CFG/PDG request.
#[derive(Clone, Debug)]
pub enum CfgTrigger {
    /// Method is an API entry point (HTTP / event-listener).
    ApiEntryPoint,
    /// Method appears on a Phase 0 taint path.
    TaintPath {
        /// The source method of the taint path.
        source: NodeId,
        /// The sink method of the taint path.
        sink: NodeId,
    },
    /// Explicit request from an MCP tool or chat query.
    ExternalRequest,
}

/// Queue of pending CFG/PDG analysis requests. Phase 1 implements the worker.
///
/// For now this is a plain `Vec` — Phase 1 will add an async worker pool and
/// an LRU result cache keyed by `(method_id, ast_hash)`.
#[derive(Default)]
pub struct CfgRequestQueue {
    pending: Vec<CfgRequest>,
}

impl CfgRequestQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue a method for on-demand CFG/PDG analysis.
    pub fn push(&mut self, request: CfgRequest) {
        self.pending.push(request);
    }

    /// Drain all pending requests. Called by the Phase 1 worker.
    pub fn drain(&mut self) -> Vec<CfgRequest> {
        std::mem::take(&mut self.pending)
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}
