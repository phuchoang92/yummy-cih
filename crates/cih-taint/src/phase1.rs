//! Phase 1 intra-procedural taint refinement.
//!
//! For each method on a Phase 0 taint path, builds an in-memory [`MethodBody`] from
//! tree-sitter AST (via [`crate::java_ir`]) and runs a simple liveness-based
//! propagation to confirm or suppress taint paths.
//!
//! # Algorithm
//!
//! 1. Seed `tainted_vars` with the names of parameters that carry tainted data
//!    (the caller decides which parameters are tainted based on what the previous
//!    hop passed in).
//! 2. Walk statements in source order:
//!    - If a statement reads a tainted variable, mark written variables as tainted.
//!    - If a statement calls a sink with a tainted argument, record a [`ConfirmedSink`].
//!    - If a `return` statement reads tainted vars, set `taint_return = true`.
//! 3. Return `taint_return` so callers can propagate taint inter-procedurally.
//!
//! Phase 1 is intentionally *flow-insensitive* (no branch splitting) and
//! *field-insensitive* (field names treated as plain variables). This is intentional —
//! the goal is to cut obvious false positives from Phase 0, not to replace a
//! full-precision analysis.

use std::collections::HashSet;

use cih_core::NodeId;

use crate::confidence::{PHASE1_CONFIRMED, PHASE1_NO_FLOW};
use crate::ir::{MethodBody, StatementKind};

// ── Result types ─────────────────────────────────────────────────────────────

/// Result of intra-procedural taint analysis for a single method.
#[derive(Clone, Debug)]
pub struct IntraResult {
    pub callable_id: NodeId,
    /// Whether tainted data reaches a `return` statement in this method.
    pub taint_return: bool,
    /// Sink call sites within this method body that receive tainted arguments.
    pub confirmed_sinks: Vec<ConfirmedSink>,
}

/// A call to a known sink that has at least one tainted argument.
#[derive(Clone, Debug)]
pub struct ConfirmedSink {
    /// Statement node ID of the call.
    pub stmt_id: NodeId,
    /// Unqualified callee name (e.g. `"execute"`, `"exec"`).
    pub call_name: String,
    /// Argument variable names that are tainted at this call site.
    pub tainted_args: Vec<String>,
}

// ── Analysis ──────────────────────────────────────────────────────────────────

/// Run intra-procedural taint analysis for `body`.
///
/// `tainted_params`: subset of `body.param_names` that carry tainted data into
/// this method. Pass all params for API entry points; for intermediate methods,
/// pass only the params the upstream call site fed tainted values into.
///
/// `sink_name_patterns`: unqualified method name substrings that identify sinks
/// (e.g. `&["execute", "exec", "write"]`). Checked with `str::contains`.
pub fn analyze_method(
    body: &MethodBody,
    tainted_params: &[String],
    sink_name_patterns: &[&str],
) -> IntraResult {
    let mut tainted: HashSet<String> = tainted_params.iter().cloned().collect();
    let mut confirmed_sinks: Vec<ConfirmedSink> = Vec::new();
    let mut taint_return = false;

    for stmt in &body.statements {
        let read_tainted = stmt.reads.iter().any(|r| tainted.contains(r.as_str()));
        let tainted_call_args: Vec<String> = stmt
            .call_args
            .iter()
            .filter(|a| tainted.contains(a.as_str()))
            .cloned()
            .collect();

        // Check: call with tainted args hits a known sink pattern.
        if !tainted_call_args.is_empty() {
            if let Some(callee) = &stmt.call_site {
                if sink_name_patterns.iter().any(|p| callee.contains(p)) {
                    confirmed_sinks.push(ConfirmedSink {
                        stmt_id: stmt.id.clone(),
                        call_name: callee.clone(),
                        tainted_args: tainted_call_args.clone(),
                    });
                }
            }
        }

        // Propagate: if reads or call args were tainted, writes become tainted.
        if read_tainted || !tainted_call_args.is_empty() {
            for w in &stmt.writes {
                tainted.insert(w.clone());
            }
        }

        // Track taint reaching return.
        if stmt.kind == StatementKind::Return && read_tainted {
            taint_return = true;
        }
    }

    IntraResult {
        callable_id: body.callable_id.clone(),
        taint_return,
        confirmed_sinks,
    }
}

// ── Path-level runner ─────────────────────────────────────────────────────────

/// Summary of Phase 1 refinement for a single Phase 0 taint path.
#[derive(Clone, Debug)]
pub struct PathRefinement {
    /// Index into the original Phase 0 `paths` vec.
    pub path_index: usize,
    /// Whether Phase 1 found a confirmed sink call along the source method's body.
    pub intra_confirmed: bool,
    /// Whether Phase 1 could not locate/parse the source method body at all.
    pub ir_unavailable: bool,
    /// Adjusted confidence multiplier to apply to the Phase 0 confidence.
    /// `1.0` = unchanged, `1.2` = bump (confirmed), `0.6` = penalise (no flow found).
    pub confidence_multiplier: f32,
}

/// Run Phase 1 on the source methods of all Phase 0 taint paths.
///
/// For each unique source method, this function:
/// 1. Resolves its source file via `resolve_src(node_file)`.
/// 2. Builds the [`MethodBody`] with [`crate::java_ir::extract_method_body`].
/// 3. Runs [`analyze_method`] with all parameters as tainted (conservative).
/// 4. If any sink call is confirmed → bump confidence; if no flow found → penalise.
///
/// Returns one [`PathRefinement`] per path (same length as `paths`).
pub fn refine_paths(
    paths: &[crate::pass::TaintPath],
    node_file: &dyn Fn(&cih_core::NodeId) -> Option<String>,
    resolve_src: impl Fn(&str) -> Option<String>,
    sink_name_patterns: &[&str],
) -> Vec<PathRefinement> {
    use std::collections::HashMap;

    // Cache IntraResult per source method (multiple paths may share a source).
    let mut intra_cache: HashMap<NodeId, Option<IntraResult>> = HashMap::new();

    paths
        .iter()
        .enumerate()
        .map(|(idx, path)| {
            let result = intra_cache
                .entry(path.source.clone())
                .or_insert_with(|| {
                    let file = node_file(&path.source)?;
                    let src = resolve_src(&file)?;
                    let body =
                        crate::java_ir::extract_method_body(&path.source, &src)?;
                    // Treat all params as tainted — we know the source is an API handler.
                    let tainted: Vec<String> = body.param_names.clone();
                    Some(analyze_method(&body, &tainted, sink_name_patterns))
                });

            match result {
                None => PathRefinement {
                    path_index: idx,
                    intra_confirmed: false,
                    ir_unavailable: true,
                    confidence_multiplier: 1.0, // can't confirm or deny
                },
                Some(intra) => {
                    let confirmed = !intra.confirmed_sinks.is_empty() || intra.taint_return;
                    PathRefinement {
                        path_index: idx,
                        intra_confirmed: confirmed,
                        ir_unavailable: false,
                        // Confirmed intra-flow → boost; no intra-flow → modest penalty.
                        confidence_multiplier: if confirmed { PHASE1_CONFIRMED } else { PHASE1_NO_FLOW },
                    }
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{MethodBody, StatementKind, StatementNode};
    use cih_core::{NodeId, Range};

    fn mid(s: &str) -> NodeId {
        NodeId::new(s)
    }

    fn stmt(
        callable: &NodeId,
        kind: StatementKind,
        reads: &[&str],
        writes: &[&str],
        call_site: Option<&str>,
        call_args: &[&str],
        byte: usize,
    ) -> StatementNode {
        StatementNode {
            id: NodeId::new(format!("{}:stmt:{byte}", callable.as_str())),
            kind,
            in_callable: callable.clone(),
            range: Range::default(),
            reads: reads.iter().map(|s| s.to_string()).collect(),
            writes: writes.iter().map(|s| s.to_string()).collect(),
            call_site: call_site.map(str::to_string),
            call_args: call_args.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn confirms_direct_tainted_sink_call() {
        // void process(String input) { jdbcTemplate.execute(input); }
        let id = mid("Method:com.example.Foo#process/1");
        let body = MethodBody {
            callable_id: id.clone(),
            param_names: vec!["input".to_string()],
            statements: vec![stmt(
                &id,
                StatementKind::Call,
                &[],
                &[],
                Some("execute"),
                &["input"],
                10,
            )],
        };

        let result = analyze_method(&body, &["input".to_string()], &["execute"]);
        assert_eq!(result.confirmed_sinks.len(), 1);
        assert_eq!(result.confirmed_sinks[0].call_name, "execute");
        assert!(result.confirmed_sinks[0].tainted_args.contains(&"input".to_string()));
    }

    #[test]
    fn propagates_taint_through_assign_then_sink() {
        // void process(String input) { String q = build(input); execute(q); }
        let id = mid("Method:com.example.Foo#process/1");
        let body = MethodBody {
            callable_id: id.clone(),
            param_names: vec!["input".to_string()],
            statements: vec![
                stmt(&id, StatementKind::Assign, &["input"], &["q"], Some("build"), &["input"], 10),
                stmt(&id, StatementKind::Call, &[], &[], Some("execute"), &["q"], 20),
            ],
        };

        let result = analyze_method(&body, &["input".to_string()], &["execute"]);
        assert_eq!(result.confirmed_sinks.len(), 1);
        assert!(result.confirmed_sinks[0].tainted_args.contains(&"q".to_string()));
    }

    #[test]
    fn no_taint_no_sink_confirmation() {
        // void process(String input) { execute(hardcoded); }  — non-tainted arg
        let id = mid("Method:com.example.Foo#process/1");
        let body = MethodBody {
            callable_id: id.clone(),
            param_names: vec!["input".to_string()],
            statements: vec![stmt(
                &id,
                StatementKind::Call,
                &[],
                &[],
                Some("execute"),
                &["hardcoded"],
                10,
            )],
        };

        let result = analyze_method(&body, &["input".to_string()], &["execute"]);
        assert!(result.confirmed_sinks.is_empty(), "hardcoded arg should not confirm sink");
    }

    #[test]
    fn taint_return_detected() {
        // String process(String input) { return input; }
        let id = mid("Method:com.example.Foo#process/1");
        let body = MethodBody {
            callable_id: id.clone(),
            param_names: vec!["input".to_string()],
            statements: vec![stmt(
                &id,
                StatementKind::Return,
                &["input"],
                &[],
                None,
                &[],
                10,
            )],
        };

        let result = analyze_method(&body, &["input".to_string()], &["execute"]);
        assert!(result.taint_return, "should detect taint reaching return");
    }
}
