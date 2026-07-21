//! Shared accounting for bounded or partially failed analysis.

use serde::Serialize;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ResultBounds {
    pub(crate) complete: bool,
    pub(crate) total_known: Option<usize>,
    pub(crate) returned: usize,
    pub(crate) omitted: Option<usize>,
    pub(crate) failed: usize,
    pub(crate) limit: Option<usize>,
    pub(crate) reasons: Vec<&'static str>,
}

impl Default for ResultBounds {
    fn default() -> Self {
        Self::exact_limit(0, 0, None)
    }
}

impl ResultBounds {
    /// Metadata for a collection that was fully loaded before a local limit was
    /// applied. Its total and omitted counts are exact.
    pub(crate) fn exact_limit(total: usize, returned: usize, limit: Option<usize>) -> Self {
        let omitted = total.saturating_sub(returned);
        Self {
            complete: omitted == 0,
            total_known: Some(total),
            returned,
            omitted: Some(omitted),
            failed: 0,
            limit,
            reasons: if omitted == 0 {
                Vec::new()
            } else {
                vec!["result_limit"]
            },
        }
    }

    /// Metadata for a backend API that accepts a limit but does not return an
    /// exact total. It is intentionally conservative: a short page is not
    /// treated as proof that the backend had no more rows.
    pub(crate) fn backend_limited(returned: usize, limit: usize) -> Self {
        Self {
            complete: false,
            total_known: None,
            returned,
            omitted: None,
            failed: 0,
            limit: Some(limit),
            reasons: vec!["backend_limit"],
        }
    }

    pub(crate) fn requested_scope(returned: usize) -> Self {
        Self {
            complete: true,
            total_known: Some(returned),
            returned,
            omitted: Some(0),
            failed: 0,
            limit: None,
            reasons: Vec::new(),
        }
    }

    pub(crate) fn partial_unknown(returned: usize, failed: usize) -> Self {
        Self {
            complete: false,
            total_known: None,
            returned,
            omitted: None,
            failed,
            limit: None,
            reasons: vec!["traversal_budget_or_dependency"],
        }
    }
}

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
