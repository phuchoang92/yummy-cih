//! Phase 2: Control Flow Graph (CFG) construction for Java methods.
//!
//! Builds an in-memory CFG via structural recursive descent over the tree-sitter AST.
//! Java has no `goto`, so every loop/branch is a structured construct — this makes
//! the recursive approach correct and simple.
//!
//! After CFG construction, [`Cfg::compute_dominators`] delegates to the
//! Cooper-Harvey-Kennedy algorithm in [`domtree`] to produce a [`DomTree`].
//! This is the foundation for Phase 3 control-dependence computation.
//!
//! Nothing in this module is persisted to the main graph.

pub(crate) mod domtree;
pub use domtree::DomTree;

use std::collections::HashMap;

use tree_sitter::{Node as TsNode, Parser};

use cih_core::NodeId;

use crate::ir::{StatementKind, StatementNode};
use crate::java_ast::{
    collect_reads, extract_call_args, extract_call_site, extract_param_names, find_method_node,
    parse_method_id, range_of, stmt_id, ts_text,
};

// ── Public types ──────────────────────────────────────────────────────────────

/// Opaque identifier for a basic block within a [`Cfg`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct BlockId(pub u32);

/// The kind of control-flow edge between two basic blocks.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum CfgEdgeKind {
    /// Normal sequential flow (no branch).
    Sequential,
    /// Branch taken (if-true, loop-continue, switch case match).
    True,
    /// Branch not taken (if-false, loop-exit).
    False,
    /// Exception edge from try-body to catch handler.
    Exception,
    /// Back edge from loop body to loop header.
    Back,
}

/// A basic block: a maximal straight-line sequence of statements.
#[derive(Debug)]
pub struct BasicBlock {
    pub id: BlockId,
    /// Statements contained in this block (using Phase 1 IR types).
    pub stmts: Vec<StatementNode>,
    /// Outgoing edges: `(target_block, edge_kind)`.
    pub succs: Vec<(BlockId, CfgEdgeKind)>,
    /// Incoming edge sources (filled after construction).
    pub preds: Vec<BlockId>,
    /// True if this block ends with a return/throw and has no fall-through.
    pub is_terminated: bool,
}

/// In-memory control flow graph for a single Java method.
pub struct Cfg {
    pub callable_id: NodeId,
    pub blocks: Vec<BasicBlock>,
    /// Entry block (always the first block that contains the first statement).
    pub entry: BlockId,
    /// Synthetic exit block (no statements; all returns/throws flow here).
    pub exit: BlockId,
    /// Formal parameter names in declaration order, extracted from the method signature.
    /// Used by Phase 3 to seed initial taint: for HTTP-handler sources, all params arrive tainted.
    pub param_names: Vec<String>,
}

impl Cfg {
    /// Number of blocks (including synthetic exit).
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Number of edges (sum of all successors).
    pub fn edge_count(&self) -> usize {
        self.blocks.iter().map(|b| b.succs.len()).sum()
    }

    /// McCabe cyclomatic complexity: E − N + 2P where P = 1 (one procedure).
    pub fn cyclomatic_complexity(&self) -> usize {
        let e = self.edge_count();
        let n = self.block_count();
        e.saturating_sub(n) + 2
    }

    /// Look up a block by id.
    pub fn block(&self, id: &BlockId) -> Option<&BasicBlock> {
        self.blocks.iter().find(|b| b.id == *id)
    }

    /// Look up a statement by its unique ID across all blocks.
    pub fn stmt_by_id(&self, id: &NodeId) -> Option<&StatementNode> {
        self.blocks
            .iter()
            .flat_map(|b| b.stmts.iter())
            .find(|s| &s.id == id)
    }

    pub fn block_idx(&self, id: &BlockId) -> Option<usize> {
        self.blocks.iter().position(|b| b.id == *id)
    }

    /// Compute the immediate-dominator tree using Cooper-Harvey-Kennedy (2001).
    ///
    /// Returns a [`DomTree`] that can answer "which block dominates which" queries.
    /// The entry block dominates all reachable blocks; the exit block dominates
    /// only itself (when reachable).
    pub fn compute_dominators(&self) -> DomTree {
        domtree::compute_dom_tree(self)
    }

    /// Reverse post-order DFS traversal starting from `entry`, following `succs`.
    /// Back edges are detected and skipped via a "on-stack" set.
    pub fn reverse_post_order(&self) -> Vec<BlockId> {
        let mut visited = HashMap::<&BlockId, bool>::new(); // true = fully processed
        let mut post_order: Vec<BlockId> = Vec::new();
        let mut stack: Vec<(&BlockId, bool)> = Vec::new(); // (id, processed?)

        stack.push((&self.entry, false));
        while let Some((id, processing)) = stack.pop() {
            if processing {
                // All children processed — push to post_order.
                visited.insert(id, true);
                post_order.push(id.clone());
                continue;
            }
            if visited.contains_key(id) {
                continue;
            }
            // Mark as "on stack" (not yet processed).
            visited.insert(id, false);
            // Push back as "to be processed" after children.
            stack.push((id, true));
            // Push children.
            if let Some(block) = self.block(id) {
                for (succ_id, _kind) in block.succs.iter().rev() {
                    // Skip back edges: if succ is "on stack" (visited=false), it's a back edge.
                    if visited.get(succ_id).copied() != Some(false) {
                        stack.push((succ_id, false));
                    }
                }
            }
        }

        post_order.reverse();
        post_order
    }
}

// ── CFG builder ───────────────────────────────────────────────────────────────

struct CfgBuilder<'src> {
    src: &'src [u8],
    callable_id: NodeId,
    blocks: Vec<BasicBlock>,
    next_id: u32,
    exit_id: BlockId,
}

impl<'src> CfgBuilder<'src> {
    fn new(callable_id: NodeId, src: &'src [u8]) -> Self {
        let mut builder = Self {
            src,
            callable_id,
            blocks: Vec::new(),
            next_id: 0,
            exit_id: BlockId(u32::MAX), // placeholder
        };
        // Entry is block 0; exit is block 1.
        let _entry = builder.alloc_block(); // BlockId(0)
        let exit = builder.alloc_block(); // BlockId(1)
        builder.exit_id = exit;
        builder
    }

    fn alloc_block(&mut self) -> BlockId {
        let id = BlockId(self.next_id);
        self.next_id += 1;
        self.blocks.push(BasicBlock {
            id: id.clone(),
            stmts: Vec::new(),
            succs: Vec::new(),
            preds: Vec::new(),
            is_terminated: false,
        });
        id
    }

    fn new_block(&mut self) -> BlockId {
        self.alloc_block()
    }

    fn block_idx(&self, id: &BlockId) -> Option<usize> {
        self.blocks.iter().position(|b| b.id == *id)
    }

    fn add_edge(&mut self, from: &BlockId, to: &BlockId, kind: CfgEdgeKind) {
        let Some(fi) = self.block_idx(from) else {
            return;
        };
        let Some(ti) = self.block_idx(to) else { return };
        // Avoid duplicate edges (can happen with dead-code blocks).
        if self.blocks[fi]
            .succs
            .iter()
            .any(|(t, k)| t == to && *k == kind)
        {
            return;
        }
        self.blocks[fi].succs.push((to.clone(), kind));
        self.blocks[ti].preds.push(from.clone());
    }

    fn add_stmt(&mut self, block: &BlockId, stmt: StatementNode) {
        let Some(idx) = self.block_idx(block) else {
            return;
        };
        self.blocks[idx].stmts.push(stmt);
    }

    fn set_terminated(&mut self, block: &BlockId) {
        let Some(idx) = self.block_idx(block) else {
            return;
        };
        self.blocks[idx].is_terminated = true;
    }

    fn is_terminated(&self, block: &BlockId) -> bool {
        self.block_idx(block)
            .is_some_and(|idx| self.blocks[idx].is_terminated)
    }

    // ── Statement node factories ────────────────────────────────────────────

    fn make_stmt(&self, node: TsNode<'_>, kind: StatementKind) -> StatementNode {
        StatementNode {
            id: stmt_id(&self.callable_id, node.start_byte()),
            kind,
            in_callable: self.callable_id.clone(),
            range: range_of(node),
            reads: Vec::new(),
            writes: Vec::new(),
            call_site: None,
            call_args: Vec::new(),
        }
    }

    fn make_branch_stmt(&self, node: TsNode<'_>) -> StatementNode {
        let mut reads = Vec::new();
        if let Some(cond) = node.child_by_field_name("condition") {
            collect_reads(cond, self.src, &mut reads);
        }
        StatementNode {
            id: stmt_id(&self.callable_id, node.start_byte()),
            kind: StatementKind::Branch,
            in_callable: self.callable_id.clone(),
            range: range_of(node),
            reads,
            writes: Vec::new(),
            call_site: None,
            call_args: Vec::new(),
        }
    }

    fn make_loop_stmt(&self, node: TsNode<'_>) -> StatementNode {
        let mut reads = Vec::new();
        // Grab condition/iterable reads but not the body.
        match node.kind() {
            "while_statement" | "do_statement" => {
                if let Some(cond) = node.child_by_field_name("condition") {
                    collect_reads(cond, self.src, &mut reads);
                }
            }
            "for_statement" => {
                let children: Vec<TsNode<'_>> = {
                    let mut cursor = node.walk();
                    node.named_children(&mut cursor).collect()
                };
                for child in children {
                    if child.kind() != "block" {
                        collect_reads(child, self.src, &mut reads);
                    }
                }
            }
            "enhanced_for_statement" => {
                if let Some(val) = node.child_by_field_name("value") {
                    collect_reads(val, self.src, &mut reads);
                }
            }
            _ => {}
        }
        StatementNode {
            id: stmt_id(&self.callable_id, node.start_byte()),
            kind: StatementKind::Loop,
            in_callable: self.callable_id.clone(),
            range: range_of(node),
            reads,
            writes: Vec::new(),
            call_site: None,
            call_args: Vec::new(),
        }
    }

    fn make_plain_stmt(&self, node: TsNode<'_>) -> StatementNode {
        let kind = classify_kind(node);
        let mut reads = Vec::new();
        let mut writes = Vec::new();
        let mut call_site = None;
        let mut call_args = Vec::new();

        match node.kind() {
            "local_variable_declaration" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.kind() == "variable_declarator" {
                        if let Some(name_node) = child.child_by_field_name("name") {
                            writes.push(ts_text(name_node, self.src).to_string());
                        }
                        if let Some(value) = child.child_by_field_name("value") {
                            if matches!(
                                value.kind(),
                                "method_invocation" | "object_creation_expression"
                            ) {
                                call_site = extract_call_site(value, self.src);
                                call_args = extract_call_args(value, self.src);
                            }
                            collect_reads(value, self.src, &mut reads);
                        }
                    }
                }
            }
            "expression_statement" => {
                // Name the iterator so it drops before `cursor` at end of inner block.
                let inner_opt: Option<TsNode<'_>> = {
                    let mut cursor = node.walk();
                    let mut it = node.named_children(&mut cursor);
                    it.next()
                };
                if let Some(inner) = inner_opt {
                    match inner.kind() {
                        "assignment_expression" | "compound_assignment_expression" => {
                            if let Some(left) = inner.child_by_field_name("left") {
                                if left.kind() == "identifier" {
                                    writes.push(ts_text(left, self.src).to_string());
                                }
                            }
                            if let Some(right) = inner.child_by_field_name("right") {
                                if matches!(
                                    right.kind(),
                                    "method_invocation" | "object_creation_expression"
                                ) {
                                    call_site = extract_call_site(right, self.src);
                                    call_args = extract_call_args(right, self.src);
                                }
                                collect_reads(right, self.src, &mut reads);
                            }
                        }
                        "method_invocation" | "object_creation_expression" => {
                            call_site = extract_call_site(inner, self.src);
                            call_args = extract_call_args(inner, self.src);
                            collect_reads(inner, self.src, &mut reads);
                        }
                        _ => collect_reads(inner, self.src, &mut reads),
                    }
                }
            }
            "return_statement" | "throw_statement" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    collect_reads(child, self.src, &mut reads);
                }
            }
            _ => {
                collect_reads(node, self.src, &mut reads);
            }
        }

        StatementNode {
            id: stmt_id(&self.callable_id, node.start_byte()),
            kind,
            in_callable: self.callable_id.clone(),
            range: range_of(node),
            reads,
            writes,
            call_site,
            call_args,
        }
    }

    // ── Recursive CFG construction ──────────────────────────────────────────
    //
    // Each `build_*` function:
    //   - Receives the current block (all preceding statements land here)
    //   - Builds subgraph for the AST node
    //   - Returns the "continuation" block (where the next statement should land)
    //
    // A block is "terminated" when it ends with a return/throw — in that case the
    // continuation block is a synthetic dead-code block.

    fn build_block_node(&mut self, block_node: TsNode<'_>, current: BlockId) -> BlockId {
        let mut cur = current;
        let mut cursor = block_node.walk();
        let children: Vec<TsNode<'_>> = block_node.named_children(&mut cursor).collect();
        for child in children {
            cur = self.build_node(child, cur);
        }
        cur
    }

    fn build_node(&mut self, node: TsNode<'_>, current: BlockId) -> BlockId {
        match node.kind() {
            "block" => self.build_block_node(node, current),
            "if_statement" => self.build_if(node, current),
            "switch_statement" | "switch_expression" => self.build_switch(node, current),
            "while_statement" => self.build_while(node, current),
            "do_statement" => self.build_do(node, current),
            "for_statement" => self.build_for(node, current),
            "enhanced_for_statement" => self.build_enhanced_for(node, current),
            "try_statement" | "try_with_resources_statement" => self.build_try(node, current),
            "synchronized_statement" => {
                let mut cursor = node.walk();
                let children: Vec<TsNode<'_>> = node.named_children(&mut cursor).collect();
                let mut cur = current;
                for child in children {
                    if child.kind() == "block" {
                        cur = self.build_block_node(child, cur);
                    }
                }
                cur
            }
            "return_statement" | "throw_statement" => {
                if !self.is_terminated(&current) {
                    let stmt = self.make_plain_stmt(node);
                    self.add_stmt(&current, stmt);
                    self.add_edge(&current, &self.exit_id.clone(), CfgEdgeKind::Sequential);
                    self.set_terminated(&current);
                }
                // Dead-code continuation block.
                self.new_block()
            }
            // Block-comments, blank lines, etc.
            "block_comment" | "line_comment" => current,
            _ => {
                if !self.is_terminated(&current) {
                    let stmt = self.make_plain_stmt(node);
                    self.add_stmt(&current, stmt);
                }
                current
            }
        }
    }

    fn build_if(&mut self, node: TsNode<'_>, current: BlockId) -> BlockId {
        let branch_stmt = self.make_branch_stmt(node);
        self.add_stmt(&current, branch_stmt);

        let then_entry = self.new_block();
        let join = self.new_block();

        self.add_edge(&current, &then_entry, CfgEdgeKind::True);

        // Then body.
        let then_exit = if let Some(then) = node.child_by_field_name("consequence") {
            self.build_node(then, then_entry.clone())
        } else {
            then_entry
        };
        if !self.is_terminated(&then_exit) {
            self.add_edge(&then_exit, &join, CfgEdgeKind::Sequential);
        }

        // Else body (or direct false edge to join).
        if let Some(alt) = node.child_by_field_name("alternative") {
            let else_entry = self.new_block();
            self.add_edge(&current, &else_entry, CfgEdgeKind::False);
            let else_exit = self.build_node(alt, else_entry);
            if !self.is_terminated(&else_exit) {
                self.add_edge(&else_exit, &join, CfgEdgeKind::Sequential);
            }
        } else {
            self.add_edge(&current, &join, CfgEdgeKind::False);
        }

        join
    }

    fn build_switch(&mut self, node: TsNode<'_>, current: BlockId) -> BlockId {
        let branch_stmt = self.make_branch_stmt(node);
        self.add_stmt(&current, branch_stmt);

        let after = self.new_block();

        if let Some(body) = node.child_by_field_name("body") {
            let mut cursor = body.walk();
            let groups: Vec<TsNode<'_>> = body.named_children(&mut cursor).collect();
            let mut case_entry: Option<BlockId> = None;
            for group in groups {
                let entry = case_entry.take().unwrap_or_else(|| {
                    let b = self.new_block();
                    self.add_edge(&current, &b, CfgEdgeKind::True);
                    b
                });
                let mut cur = entry.clone();
                let mut cursor2 = group.walk();
                let stmts: Vec<TsNode<'_>> = group.named_children(&mut cursor2).collect();
                for stmt in stmts {
                    if !matches!(stmt.kind(), "switch_label" | "switch_rule_expression") {
                        cur = self.build_node(stmt, cur);
                    }
                }
                // Fall-through to the next case.
                case_entry = Some(cur);
            }
            // Last case falls through to after.
            if let Some(last) = case_entry {
                if !self.is_terminated(&last) {
                    self.add_edge(&last, &after, CfgEdgeKind::Sequential);
                }
            }
        }
        // Default case → after (if no explicit default).
        self.add_edge(&current, &after, CfgEdgeKind::False);

        after
    }

    fn build_while(&mut self, node: TsNode<'_>, current: BlockId) -> BlockId {
        let header = self.new_block();
        let body_entry = self.new_block();
        let after = self.new_block();

        self.add_edge(&current, &header, CfgEdgeKind::Sequential);
        let loop_stmt = self.make_loop_stmt(node);
        self.add_stmt(&header, loop_stmt);
        self.add_edge(&header, &body_entry, CfgEdgeKind::True);
        self.add_edge(&header, &after, CfgEdgeKind::False);

        if let Some(body) = node.child_by_field_name("body") {
            let body_exit = self.build_node(body, body_entry);
            if !self.is_terminated(&body_exit) {
                self.add_edge(&body_exit, &header, CfgEdgeKind::Back);
            }
        }

        after
    }

    fn build_do(&mut self, node: TsNode<'_>, current: BlockId) -> BlockId {
        // do { body } while (cond);
        // current → body_entry; body_tail → header (cond); header → body_entry (back) or after
        let body_entry = self.new_block();
        let header = self.new_block(); // condition check at end
        let after = self.new_block();

        self.add_edge(&current, &body_entry, CfgEdgeKind::Sequential);

        let body_exit = if let Some(body) = node.child_by_field_name("body") {
            self.build_node(body, body_entry.clone())
        } else {
            body_entry.clone()
        };

        if !self.is_terminated(&body_exit) {
            self.add_edge(&body_exit, &header, CfgEdgeKind::Sequential);
        }

        let cond_stmt = self.make_loop_stmt(node);
        self.add_stmt(&header, cond_stmt);
        self.add_edge(&header, &body_entry, CfgEdgeKind::Back);
        self.add_edge(&header, &after, CfgEdgeKind::False);

        after
    }

    fn build_for(&mut self, node: TsNode<'_>, current: BlockId) -> BlockId {
        // for (init; cond; update) { body }
        // current → [init_block] → [header: cond] → body_entry → ... → [update_block] → header (back)
        //                                          → after (false)
        let header = self.new_block();
        let body_entry = self.new_block();
        let after = self.new_block();

        self.add_edge(&current, &header, CfgEdgeKind::Sequential);
        let loop_stmt = self.make_loop_stmt(node);
        self.add_stmt(&header, loop_stmt);
        self.add_edge(&header, &body_entry, CfgEdgeKind::True);
        self.add_edge(&header, &after, CfgEdgeKind::False);

        if let Some(body) = node.child_by_field_name("body") {
            let body_exit = self.build_node(body, body_entry);
            if !self.is_terminated(&body_exit) {
                self.add_edge(&body_exit, &header, CfgEdgeKind::Back);
            }
        }

        after
    }

    fn build_enhanced_for(&mut self, node: TsNode<'_>, current: BlockId) -> BlockId {
        // for (Type x : iterable) { body }
        let header = self.new_block();
        let body_entry = self.new_block();
        let after = self.new_block();

        self.add_edge(&current, &header, CfgEdgeKind::Sequential);
        let loop_stmt = self.make_loop_stmt(node);
        self.add_stmt(&header, loop_stmt);
        self.add_edge(&header, &body_entry, CfgEdgeKind::True);
        self.add_edge(&header, &after, CfgEdgeKind::False);

        if let Some(body) = node.child_by_field_name("body") {
            let body_exit = self.build_node(body, body_entry);
            if !self.is_terminated(&body_exit) {
                self.add_edge(&body_exit, &header, CfgEdgeKind::Back);
            }
        }

        after
    }

    fn build_try(&mut self, node: TsNode<'_>, current: BlockId) -> BlockId {
        // try { body } catch (E e) { handler } finally { fin }
        let try_entry = self.new_block();
        let after = self.new_block();
        let catch_entry = self.new_block();

        let try_stmt = self.make_stmt(node, StatementKind::Try);
        self.add_stmt(&current, try_stmt);
        self.add_edge(&current, &try_entry, CfgEdgeKind::Sequential);
        // Exception edge from try body to catch.
        self.add_edge(&try_entry, &catch_entry, CfgEdgeKind::Exception);

        let mut cursor = node.walk();
        let children: Vec<TsNode<'_>> = node.named_children(&mut cursor).collect();

        // Build try body.
        let try_exit = {
            let mut cur = try_entry.clone();
            for child in &children {
                if (child.kind() == "block" || child.kind() == "resource_specification")
                    && child.kind() == "block"
                {
                    cur = self.build_block_node(*child, cur);
                    break;
                }
            }
            cur
        };
        if !self.is_terminated(&try_exit) {
            self.add_edge(&try_exit, &after, CfgEdgeKind::Sequential);
        }

        // Build catch clauses.
        let mut catch_exits: Vec<BlockId> = Vec::new();
        for child in &children {
            if child.kind() == "catch_clause" {
                if let Some(body) = child.child_by_field_name("body") {
                    let catch_exit = self.build_node(body, catch_entry.clone());
                    if !self.is_terminated(&catch_exit) {
                        catch_exits.push(catch_exit);
                    }
                }
            }
        }

        // Finally clause (if present): all paths merge into finally before after.
        let final_entry_opt = children.iter().find(|c| c.kind() == "finally_clause");
        if let Some(finally) = final_entry_opt {
            let fin_entry = self.new_block();
            // `after` is the merge point for both the normal try exit and all catch exits.
            // Wire it to finally so the dominator tree sees a single entry into the finally block.
            self.add_edge(&after, &fin_entry, CfgEdgeKind::Sequential);
            for ce in &catch_exits {
                // Catch exits go through `after` (not directly to fin_entry) so that `after`
                // is reachable from both paths and the dominator tree is correct.
                self.add_edge(ce, &after, CfgEdgeKind::Sequential);
            }
            let fin_children: Vec<TsNode<'_>> = {
                let mut cursor2 = finally.walk();
                finally.named_children(&mut cursor2).collect()
            };
            let mut fin_cur = fin_entry;
            for fc in fin_children {
                if fc.kind() == "block" {
                    fin_cur = self.build_block_node(fc, fin_cur);
                    break;
                }
            }
            // after here is the continuation after the whole try-catch-finally
            let real_after = self.new_block();
            if !self.is_terminated(&fin_cur) {
                self.add_edge(&fin_cur, &real_after, CfgEdgeKind::Sequential);
            }
            return real_after;
        }

        // No finally — catch exits merge to after.
        for ce in catch_exits {
            self.add_edge(&ce, &after, CfgEdgeKind::Sequential);
        }

        after
    }
}

// ── Statement kind classifier ─────────────────────────────────────────────────

fn classify_kind(node: TsNode<'_>) -> StatementKind {
    match node.kind() {
        "local_variable_declaration"
        | "assignment_expression"
        | "compound_assignment_expression" => StatementKind::Assign,
        "expression_statement" => {
            // Name the iterator so it drops before cursor (borrow released before cursor drops).
            let first_kind: Option<&str> = {
                let mut cursor = node.walk();
                let mut it = node.named_children(&mut cursor);
                it.next().map(|n| n.kind())
            };
            match first_kind {
                Some("method_invocation") | Some("object_creation_expression") => {
                    StatementKind::Call
                }
                Some("assignment_expression") | Some("compound_assignment_expression") => {
                    StatementKind::Assign
                }
                _ => StatementKind::Other,
            }
        }
        "return_statement" => StatementKind::Return,
        "throw_statement" => StatementKind::Throw,
        "try_statement" | "try_with_resources_statement" => StatementKind::Try,
        "if_statement" | "switch_statement" | "switch_expression" => StatementKind::Branch,
        "while_statement" | "for_statement" | "enhanced_for_statement" | "do_statement" => {
            StatementKind::Loop
        }
        _ => StatementKind::Other,
    }
}

// ── Public constructor ────────────────────────────────────────────────────────

/// Build an in-memory CFG for the Java method identified by `method_id`.
///
/// `src` is the full text of the source file containing the method. Returns
/// `None` if tree-sitter fails to parse, or the method is not found in the AST.
pub fn build_cfg(method_id: &NodeId, src: &str) -> Option<Cfg> {
    let (target_name, target_arity) = parse_method_id(method_id)?;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(src.as_bytes(), None)?;
    let root = tree.root_node();

    let method_node = find_method_node(root, src.as_bytes(), &target_name, target_arity)?;
    let body = method_node.child_by_field_name("body")?;

    let param_names = method_node
        .child_by_field_name("parameters")
        .map(|p| extract_param_names(p, src.as_bytes()))
        .unwrap_or_default();

    let mut builder = CfgBuilder::new(method_id.clone(), src.as_bytes());
    let entry = BlockId(0);
    let exit = builder.exit_id.clone();

    let last = builder.build_block_node(body, entry.clone());
    if !builder.is_terminated(&last) {
        let ex = exit.clone();
        builder.add_edge(&last, &ex, CfgEdgeKind::Sequential);
    }

    Some(Cfg {
        callable_id: method_id.clone(),
        blocks: builder.blocks,
        entry,
        exit,
        param_names,
    })
}
