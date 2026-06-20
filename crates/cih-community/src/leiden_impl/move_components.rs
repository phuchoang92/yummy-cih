#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Parameters for computing the quality delta of moving a node between communities.
///
/// Unified for directed and undirected graphs:
/// - Undirected: `_in` fields are all `0.0`, `directed` is `false`. Quality
///   functions use the `_out` fields with the classic undirected formula.
/// - Directed: `_in` fields contain in-edge statistics, `directed` is `true`.
///   Quality functions use both `_out` and `_in` fields.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct MoveComponents {
    // ── Global statistics ──
    /// Twice the total edge weight of the graph (`2m`).
    pub two_m: f64,
    /// Weight of the node being moved.
    pub node_weight: f64,
    /// Total node weight across all communities.
    pub total_node_weight: f64,

    // ── Out-edge statistics ──
    /// Weighted out-degree of the node being moved.
    pub k_v_out: f64,
    /// Out-edge weight from the node to the target community.
    pub k_v_to_target_out: f64,
    /// Out-edge weight from the node to its current community.
    pub k_v_to_current_out: f64,
    /// Total weighted out-degree of the target community.
    pub sigma_tot_target_out: f64,
    /// Total weighted out-degree of the current community.
    pub sigma_tot_current_out: f64,

    // ── In-edge statistics (0.0 for undirected graphs) ──
    /// Weighted in-degree of the node being moved.
    pub k_v_in: f64,
    /// In-edge weight from the node to the target community.
    pub k_v_to_target_in: f64,
    /// In-edge weight from the node to its current community.
    pub k_v_to_current_in: f64,
    /// Total weighted in-degree of the target community.
    pub sigma_tot_target_in: f64,
    /// Total weighted in-degree of the current community.
    pub sigma_tot_current_in: f64,

    // ── Community size (used by CPM/RBER) ──
    /// Total node weight in the target community.
    pub n_target: f64,
    /// Total node weight in the current community.
    pub n_current: f64,

    /// Whether the graph is directed.
    pub directed: bool,
}
