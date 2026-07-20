use std::collections::HashSet;

use cih_core::Node;
use cih_graph_store::Direction;
use rmcp::{model::CallToolResult, ErrorData as McpError};
use serde::Serialize;

use crate::args::DetectChangesArgs;
use crate::blocking::{blocking_timeout, run_blocking};
use crate::repo_context::RepoContext;
use crate::symbol::git_changed_files;
use crate::utils::{json_result, to_mcp};

/// Upper bound on changed symbols whose blast radius is traversed, read once
/// from `CIH_DETECT_CHANGES_MAX_SYMBOLS` (unset/invalid/0 = 200). Symbols over
/// the budget are reported as omitted, never silently dropped.
fn symbol_budget() -> usize {
    static BUDGET: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        std::env::var("CIH_DETECT_CHANGES_MAX_SYMBOLS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(200)
    })
}

/// Traversals in flight per batch. The store's query semaphore is the final
/// backpressure; this bounds JoinSet growth for large change sets.
const TRAVERSAL_BATCH: usize = 20;

/// Deterministic candidate order: sort by NodeId and drop duplicates, so the
/// budgeted prefix is stable regardless of store response order and a symbol
/// changed in several files is traversed once.
fn canonicalize_candidates(mut nodes: Vec<Node>) -> Vec<Node> {
    nodes.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    nodes.dedup_by(|a, b| a.id.as_str() == b.id.as_str());
    nodes
}

#[derive(Serialize)]
struct Completeness {
    /// True only when every changed symbol's blast radius was computed.
    complete: bool,
    total_candidates: usize,
    analyzed: usize,
    omitted: usize,
    failed: usize,
    /// Why analysis is incomplete: `symbol_budget` and/or `traversal_failed`.
    reasons: Vec<&'static str>,
}

/// Accounting for a budgeted run: `attempted = min(total, budget)` traversals
/// ran, of which `failed` errored; the rest of `total` was omitted. Invariant:
/// `complete == (omitted + failed == 0)` — a response can never claim
/// completeness while any candidate was skipped.
fn completeness(total: usize, attempted: usize, failed: usize) -> Completeness {
    let omitted = total.saturating_sub(attempted);
    let mut reasons = Vec::new();
    if omitted > 0 {
        reasons.push("symbol_budget");
    }
    if failed > 0 {
        reasons.push("traversal_failed");
    }
    Completeness {
        complete: omitted == 0 && failed == 0,
        total_candidates: total,
        analyzed: attempted.saturating_sub(failed),
        omitted,
        failed,
        reasons,
    }
}

#[derive(Serialize)]
struct ChangedSymbol {
    id: String,
    kind: String,
    name: String,
    file: String,
}

#[derive(Serialize)]
struct Out {
    changed_files: Vec<String>,
    changed_symbols: Vec<ChangedSymbol>,
    affected_symbols: Vec<String>,
    affected_processes: Vec<String>,
    /// Risk from the traversals that ran — a lower bound whenever
    /// `completeness.complete` is false (never inflated; see the design record).
    risk: &'static str,
    /// Explicit alias of `risk` naming its lower-bound semantics for clients.
    risk_lower_bound: &'static str,
    /// False when any candidate was omitted (budget) or failed.
    risk_complete: bool,
    /// True whenever any changed symbol's blast radius was not computed.
    partial: bool,
    /// Changed symbols with no computed blast radius (omitted + failed).
    incomplete_symbols: usize,
    completeness: Completeness,
}

pub async fn detect_changes(
    context: &RepoContext,
    args: DetectChangesArgs,
) -> Result<CallToolResult, McpError> {
    let store = &context.store;
    let repo_path = context.repo.canonical_path.display().to_string();

    // `git diff` is synchronous process I/O — run it on the blocking pool with
    // the standard deadline instead of on the async worker.
    let scope = args.scope;
    let base_ref = (!args.base_ref.is_empty()).then(|| args.base_ref.clone());
    let repo_for_git = repo_path.clone();
    let changed_files = run_blocking(blocking_timeout(), "git diff", move || {
        git_changed_files(&repo_for_git, scope, base_ref.as_deref())
    })
    .await?
    .map_err(|e| McpError::internal_error(e, None))?;

    if changed_files.is_empty() {
        return json_result(&Out {
            changed_files,
            changed_symbols: vec![],
            affected_symbols: vec![],
            affected_processes: vec![],
            risk: "none",
            risk_lower_bound: "none",
            risk_complete: true,
            partial: false,
            incomplete_symbols: 0,
            completeness: completeness(0, 0, 0),
        });
    }

    let changed_nodes =
        canonicalize_candidates(store.nodes_in_files(&changed_files).await.map_err(to_mcp)?);

    // Budgeted, batched blast-radius fan-out: at most TRAVERSAL_BATCH
    // traversals in flight, candidates beyond the budget accounted as omitted.
    // Results merge into a set, so completion order is moot.
    let attempted = changed_nodes.len().min(symbol_budget());
    let mut affected_set: HashSet<String> = HashSet::new();
    let mut failed_traversals = 0usize;
    for batch in changed_nodes[..attempted].chunks(TRAVERSAL_BATCH) {
        let mut set = tokio::task::JoinSet::new();
        for node in batch {
            let store = store.clone();
            let id = node.id.clone();
            set.spawn(async move { store.impact(&id, Direction::Upstream, 4).await });
        }
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(Ok(impact)) => {
                    for n in &impact.affected {
                        affected_set.insert(n.id.to_string());
                    }
                }
                _ => failed_traversals += 1,
            }
        }
    }
    for node in &changed_nodes {
        affected_set.remove(node.id.as_str());
    }
    let mut affected_symbols: Vec<String> = affected_set.into_iter().collect();
    affected_symbols.sort();

    let changed_ids: Vec<cih_core::NodeId> = changed_nodes.iter().map(|n| n.id.clone()).collect();
    let affected_processes = store
        .processes_for_symbols(&changed_ids)
        .await
        .map_err(to_mcp)?;

    let comp = completeness(changed_nodes.len(), attempted, failed_traversals);
    let risk = cih_graph_store::risk_from_fanout(affected_symbols.len());

    let changed_symbols: Vec<ChangedSymbol> = changed_nodes
        .iter()
        .map(|n| ChangedSymbol {
            id: n.id.to_string(),
            kind: n.kind.label().to_string(),
            name: n.name.clone(),
            file: n.file.clone(),
        })
        .collect();

    json_result(&Out {
        changed_files,
        changed_symbols,
        affected_symbols,
        affected_processes,
        risk,
        risk_lower_bound: risk,
        risk_complete: comp.complete,
        partial: !comp.complete,
        incomplete_symbols: comp.omitted + comp.failed,
        completeness: comp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{NodeId, NodeKind};

    fn node(id: &str) -> Node {
        Node {
            id: NodeId::new(id.to_string()),
            kind: NodeKind::Method,
            name: id.to_string(),
            qualified_name: None,
            file: String::new(),
            range: Default::default(),
            props: None,
        }
    }

    /// The S2 regression: 25 candidates under a budget of 20 must surface 5
    /// omitted symbols — the previous accounting reported `partial: false`
    /// here because it only counted failed sub-queries.
    #[test]
    fn over_budget_candidates_are_reported_omitted() {
        let c = completeness(25, 20, 0);
        assert!(!c.complete);
        assert_eq!(c.total_candidates, 25);
        assert_eq!(c.analyzed, 20);
        assert_eq!(c.omitted, 5);
        assert_eq!(c.failed, 0);
        assert_eq!(c.reasons, vec!["symbol_budget"]);
    }

    #[test]
    fn failed_traversals_are_reported() {
        let c = completeness(10, 10, 3);
        assert!(!c.complete);
        assert_eq!(c.analyzed, 7);
        assert_eq!(c.omitted, 0);
        assert_eq!(c.failed, 3);
        assert_eq!(c.reasons, vec!["traversal_failed"]);
    }

    #[test]
    fn full_analysis_is_complete() {
        let c = completeness(10, 10, 0);
        assert!(c.complete);
        assert!(c.reasons.is_empty());
        assert_eq!(c.analyzed, 10);
    }

    /// Design-record invariant: no result may claim completeness while
    /// `omitted + failed > 0`, and the three buckets always partition the
    /// candidate set.
    #[test]
    fn never_complete_when_anything_was_skipped() {
        for total in [0usize, 1, 19, 20, 21, 200, 201, 350] {
            for attempted in 0..=total.min(30) {
                for failed in 0..=attempted {
                    let c = completeness(total, attempted, failed);
                    let skipped = (total - attempted) + failed;
                    assert_eq!(
                        c.complete,
                        skipped == 0,
                        "total={total} attempted={attempted} failed={failed}"
                    );
                    assert_eq!(c.analyzed + c.omitted + c.failed, total);
                }
            }
        }
    }

    #[test]
    fn candidates_sort_and_dedup_deterministically() {
        let out =
            canonicalize_candidates(vec![node("b"), node("a"), node("b"), node("c"), node("a")]);
        let ids: Vec<&str> = out.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }
}
