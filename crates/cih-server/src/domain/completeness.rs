//! Shared accounting for bounded or partially failed analysis.

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Completeness {
    pub(crate) complete: bool,
    pub(crate) total_candidates: usize,
    pub(crate) analyzed: usize,
    pub(crate) omitted: usize,
    pub(crate) failed: usize,
    pub(crate) reasons: Vec<&'static str>,
}

impl Completeness {
    pub(crate) fn from_work(total: usize, attempted: usize, failed: usize) -> Self {
        let omitted = total.saturating_sub(attempted);
        let mut reasons = Vec::new();
        if omitted > 0 {
            reasons.push("symbol_budget");
        }
        if failed > 0 {
            reasons.push("traversal_failed");
        }
        Self {
            complete: omitted == 0 && failed == 0,
            total_candidates: total,
            analyzed: attempted.saturating_sub(failed),
            omitted,
            failed,
            reasons,
        }
    }
}
