//! Shared served-or-unavailable section envelope for composite tool responses
//! (`architecture_overview`, `doc_pack`). The serialized shape is pinned by
//! architecture-overview's golden tests — changing it changes every composite
//! tool's wire contract at once.

use serde::Serialize;

use cih_graph_store::GraphStoreError;

/// A section that is either served (with a one-word `source` label) or
/// explicitly unavailable with a reason + remedy. A requested section always
/// appears — `available: false` means "a pipeline step has not run" or "a query
/// failed", never "none found" (agents must not read absence as a codebase fact).
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum Section<T: Serialize> {
    Available {
        available: bool,
        /// One of: graph | registry | artifact | wiki-live | wiki-bundle (D4).
        source: &'static str,
        #[serde(flatten)]
        body: T,
    },
    Unavailable {
        available: bool,
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        remedy: Option<String>,
    },
}

impl<T: Serialize> Section<T> {
    pub(crate) fn ok(source: &'static str, body: T) -> Self {
        Self::Available {
            available: true,
            source,
            body,
        }
    }

    pub(crate) fn off(reason: impl Into<String>, remedy: Option<String>) -> Self {
        Self::Unavailable {
            available: false,
            reason: reason.into(),
            remedy,
        }
    }

    /// Backend failure on a non-first query: per-section error, worded so an
    /// outage cannot masquerade as "discover never ran" (D5 error taxonomy).
    pub(crate) fn store_err(e: &GraphStoreError) -> Self {
        Self::off(
            format!("graph query failed: {e}"),
            Some("check the graph backend / server logs — this is a serving error, not a fact about the codebase".into()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize)]
    struct Body {
        items: Vec<u32>,
    }

    /// Pins the exact wire shape the move out of architecture_overview must
    /// preserve: flattened body with `available`/`source`, and the
    /// reason/remedy form with `remedy` omitted when absent.
    #[test]
    fn serialized_shapes_are_unchanged_after_the_move() {
        let available = Section::ok("graph", Body { items: vec![1, 2] });
        assert_eq!(
            serde_json::to_value(&available).unwrap(),
            serde_json::json!({"available": true, "source": "graph", "items": [1, 2]})
        );
        let unavailable: Section<Body> = Section::off("discover has not run", None);
        assert_eq!(
            serde_json::to_value(&unavailable).unwrap(),
            serde_json::json!({"available": false, "reason": "discover has not run"})
        );
        let with_remedy: Section<Body> = Section::off("stale", Some("re-run".into()));
        assert_eq!(
            serde_json::to_value(&with_remedy).unwrap(),
            serde_json::json!({"available": false, "reason": "stale", "remedy": "re-run"})
        );
    }
}
