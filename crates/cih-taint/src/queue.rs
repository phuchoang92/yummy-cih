//! Demand-driven CFG/PDG work-item queue.
//!
//! This is a simple FIFO buffer used to coordinate which methods need
//! intra-procedural analysis (Phases 1–3). The caller drains it and processes items
//! synchronously in the current CLI batch run.
//!
//! There is no async worker pool, no LRU result cache, and no eviction logic —
//! those are appropriate only for an incremental/daemon mode where source files change
//! between requests. The current synchronous model is correct for CLI batch runs.
//!
//! Trigger conditions:
//!  1. The method is an API entry point (`HandlesRoute` / `ListensTo`).
//!  2. The method appears on a Phase 0 taint path.
//!  3. An explicit external request (MCP tool or chat query).

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

/// FIFO queue of pending intra-procedural analysis requests.
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
