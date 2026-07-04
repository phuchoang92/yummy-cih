//! In-memory statement-level IR for Phase 1 intra-procedural taint analysis.
//!
//! These types are NEVER persisted to the main graph — they live only for the
//! duration of the on-demand CFG/PDG analysis request triggered via
//! [`crate::queue::CfgRequestQueue`].

use cih_core::{NodeId, Range};

/// Classification of a single statement in a method body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatementKind {
    /// Variable declaration (`Type x = expr;`) or assignment (`x = expr;`).
    Assign,
    /// A stand-alone method call (`foo.bar(args);`).
    Call,
    /// A `return expr;` statement.
    Return,
    /// A conditional branch (`if`, `switch`).
    Branch,
    /// A loop body header (`for`, `while`, `do-while`, `for-each`).
    Loop,
    /// A `throw expr;` statement.
    Throw,
    /// A `try { … } catch { … }` block header.
    Try,
    /// Anything else (assert, break, continue, labeled, synchronized, …).
    Other,
}

/// One statement in the in-memory IR for a method body.
#[derive(Clone, Debug)]
pub struct StatementNode {
    /// Unique ID within the analysis run: `"{callable_id}:stmt:{start_byte}"`.
    pub id: NodeId,
    pub kind: StatementKind,
    /// Node ID of the containing method/constructor.
    pub in_callable: NodeId,
    /// Source location (1-based line, 0-based col).
    pub range: Range,
    /// Variable/field names that are read in this statement (best-effort).
    pub reads: Vec<String>,
    /// Variable/field names that are written in this statement (best-effort).
    pub writes: Vec<String>,
    /// Unqualified callee name when `kind == Call` or `kind == Assign` with a call on RHS.
    pub call_site: Option<String>,
    /// Identifier names extracted from the call arguments.
    pub call_args: Vec<String>,
}

/// In-memory representation of a parsed method body.
#[derive(Clone, Debug)]
pub struct MethodBody {
    /// The method's graph node ID (e.g. `Method:com.example.Foo#bar/2`).
    pub callable_id: NodeId,
    /// Parameter names declared in the method signature — taint entry points when
    /// this method is reachable from an API source.
    pub param_names: Vec<String>,
    /// Statements in source order. Nested block statements (e.g. bodies of `if`/`for`)
    /// are flattened — Phase 1 tracks variable liveness without control-flow branching.
    pub statements: Vec<StatementNode>,
}
