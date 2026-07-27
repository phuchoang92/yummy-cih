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

    /// Metadata for an offset/limit page over a store-side walk. Honest about
    /// truncation: `complete` is false whenever hops beyond this page exist.
    pub(crate) fn paged(
        returned: usize,
        offset: usize,
        has_more: bool,
        limit: usize,
        traversal_truncated: bool,
        db_effects_complete: bool,
    ) -> Self {
        let mut reasons = Vec::new();
        if offset > 0 {
            reasons.push("page_offset");
        }
        if has_more {
            reasons.push("result_limit");
        }
        if traversal_truncated {
            reasons.push("traversal_budget");
        }
        if !db_effects_complete {
            reasons.push("db_effects_unavailable");
        }
        Self {
            complete: reasons.is_empty(),
            total_known: None,
            returned,
            omitted: None,
            failed: usize::from(!db_effects_complete),
            limit: Some(limit),
            reasons,
        }
    }

    /// Metadata for a bounded shortest-path search.
    pub(crate) fn traversal(
        returned: usize,
        has_more: bool,
        traversal_truncated: bool,
        limit: usize,
    ) -> Self {
        let mut reasons = Vec::new();
        if has_more {
            reasons.push("result_limit");
        }
        if traversal_truncated {
            reasons.push("traversal_budget");
        }
        Self {
            complete: reasons.is_empty(),
            total_known: None,
            returned,
            omitted: None,
            failed: 0,
            limit: Some(limit),
            reasons,
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

    /// Record that the candidate set itself was capped by a store-side load limit
    /// before any analysis ran, so `total_candidates` is a lower bound and more
    /// candidates certainly exist. Forces `complete = false`.
    pub(crate) fn with_truncated_candidates(mut self) -> Self {
        if !self.reasons.contains(&"candidate_load_limit") {
            self.reasons.push("candidate_load_limit");
        }
        self.complete = false;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{Completeness, ResultBounds};

    #[test]
    fn truncated_candidate_load_is_never_reported_complete() {
        // A fully-analyzed page (omitted == 0, failed == 0) would be `complete`,
        // but if the candidate set itself was capped before analysis the report
        // must flip to incomplete and record the load-limit reason.
        let base = Completeness::from_work(5000, 5000, 0);
        assert!(base.complete);
        let truncated = base.with_truncated_candidates();
        assert!(!truncated.complete);
        assert!(truncated.reasons.contains(&"candidate_load_limit"));
    }

    #[test]
    fn truncated_candidate_reason_is_not_duplicated() {
        let comp = Completeness::from_work(10, 10, 0)
            .with_truncated_candidates()
            .with_truncated_candidates();
        assert_eq!(
            comp.reasons
                .iter()
                .filter(|reason| **reason == "candidate_load_limit")
                .count(),
            1
        );
    }

    #[test]
    fn trace_page_reports_every_independent_incompleteness_reason() {
        let bounds = ResultBounds::paged(10, 10, true, 10, true, false);
        assert!(!bounds.complete);
        assert_eq!(
            bounds.reasons,
            vec![
                "page_offset",
                "result_limit",
                "traversal_budget",
                "db_effects_unavailable"
            ]
        );
        assert_eq!(bounds.failed, 1);
    }

    #[test]
    fn first_complete_trace_page_is_complete() {
        let bounds = ResultBounds::paged(4, 0, false, 100, false, true);
        assert!(bounds.complete);
        assert!(bounds.reasons.is_empty());
    }

    #[test]
    fn bounded_path_search_never_calls_budget_exhaustion_not_reachable() {
        let bounds = ResultBounds::traversal(0, false, true, 3);
        assert!(!bounds.complete);
        assert_eq!(bounds.reasons, vec!["traversal_budget"]);
    }
}
