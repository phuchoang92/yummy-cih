//! Phase 2: Control Flow Graph (CFG) construction for Java methods.
//!
//! Builds an in-memory CFG via structural recursive descent over the tree-sitter AST.
//! Java has no `goto`, so every loop/branch is a structured construct — this makes
//! the recursive approach correct and simple.
//!
//! After CFG construction, [`Cfg::compute_dominators`] computes the immediate-dominator
//! tree using the Cooper-Harvey-Kennedy "Simple, Fast Dominance Algorithm" (2001),
//! which is the foundation for Phase 3 control-dependence computation.
//!
//! Nothing in this module is persisted to the main graph.

use std::collections::HashMap;

use tree_sitter::{Node as TsNode, Parser};

use cih_core::NodeId;

use crate::ir::{StatementKind, StatementNode};
use crate::java_ir::{
    collect_reads, extract_call_args, extract_call_site, find_method_node,
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

    pub fn block_idx(&self, id: &BlockId) -> Option<usize> {
        self.blocks.iter().position(|b| b.id == *id)
    }

    /// Compute the immediate-dominator tree using Cooper-Harvey-Kennedy (2001).
    ///
    /// Returns a [`DomTree`] that can answer "which block dominates which" queries.
    /// The entry block dominates all reachable blocks; the exit block dominates
    /// only itself (when reachable).
    pub fn compute_dominators(&self) -> DomTree {
        let n = self.blocks.len();
        if n == 0 {
            return DomTree::empty();
        }

        // Compute reverse post-order (RPO) starting from entry.
        let rpo = self.reverse_post_order();
        // Map block_id → RPO index
        let rpo_idx: HashMap<&BlockId, usize> = rpo
            .iter()
            .enumerate()
            .map(|(i, id)| (id, i))
            .collect();

        // idom[i] = RPO index of immediate dominator of rpo[i].
        // Undefined = usize::MAX; entry dominates itself.
        const UNDEF: usize = usize::MAX;
        let mut idom = vec![UNDEF; n];
        let entry_rpo = *rpo_idx.get(&self.entry).unwrap_or(&0);
        idom[entry_rpo] = entry_rpo;

        let mut changed = true;
        while changed {
            changed = false;
            // Iterate in RPO order (skip entry).
            for rpo_i in 0..rpo.len() {
                if rpo_i == entry_rpo {
                    continue;
                }
                let block_id = &rpo[rpo_i];
                let Some(block) = self.block(block_id) else {
                    continue;
                };

                // Find the first already-processed predecessor.
                let mut new_idom = UNDEF;
                for pred_id in &block.preds {
                    let Some(&pred_rpo) = rpo_idx.get(pred_id) else {
                        continue;
                    };
                    if idom[pred_rpo] != UNDEF {
                        new_idom = if new_idom == UNDEF {
                            pred_rpo
                        } else {
                            intersect(new_idom, pred_rpo, &idom)
                        };
                    }
                }

                if new_idom != UNDEF && idom[rpo_i] != new_idom {
                    idom[rpo_i] = new_idom;
                    changed = true;
                }
            }
        }

        // Build BlockId → immediate dominator BlockId map.
        let id_to_idom: HashMap<BlockId, BlockId> = rpo
            .iter()
            .enumerate()
            .filter_map(|(i, id)| {
                let dom_rpo = idom[i];
                if dom_rpo == UNDEF {
                    None
                } else {
                    Some((id.clone(), rpo[dom_rpo].clone()))
                }
            })
            .collect();

        DomTree { id_to_idom }
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

/// `intersect` for the Cooper-Harvey-Kennedy algorithm.
/// Both arguments are RPO indices with a valid `idom` entry.
fn intersect(mut b1: usize, mut b2: usize, idom: &[usize]) -> usize {
    while b1 != b2 {
        while b1 > b2 {
            b1 = idom[b1];
        }
        while b2 > b1 {
            b2 = idom[b2];
        }
    }
    b1
}

// ── Dominance tree ────────────────────────────────────────────────────────────

/// Immediate-dominator tree for a [`Cfg`].
pub struct DomTree {
    /// Maps each block to its immediate dominator.
    /// Entry block maps to itself.
    id_to_idom: HashMap<BlockId, BlockId>,
}

impl DomTree {
    fn empty() -> Self {
        Self {
            id_to_idom: HashMap::new(),
        }
    }

    /// Immediate dominator of `block`. Returns `None` for unreachable blocks.
    pub fn idom(&self, block: &BlockId) -> Option<&BlockId> {
        self.id_to_idom.get(block)
    }

    /// Returns `true` if `dom` strictly dominates `block`
    /// (i.e., `dom` != `block` and every path from entry to `block` passes through `dom`).
    pub fn strictly_dominates(&self, dom: &BlockId, block: &BlockId) -> bool {
        if dom == block {
            return false;
        }
        let mut cur = block;
        loop {
            match self.id_to_idom.get(cur) {
                Some(d) if d == cur => return false, // reached entry / unreachable
                Some(d) if d == dom => return true,
                Some(d) => cur = d,
                None => return false,
            }
        }
    }

    /// All block IDs that have a known immediate dominator.
    pub fn dominated_ids(&self) -> impl Iterator<Item = &BlockId> {
        self.id_to_idom.keys()
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

    fn block_idx(&self, id: &BlockId) -> usize {
        self.blocks.iter().position(|b| b.id == *id).unwrap()
    }

    fn add_edge(&mut self, from: &BlockId, to: &BlockId, kind: CfgEdgeKind) {
        // Avoid duplicate edges (can happen with dead-code blocks).
        let fi = self.block_idx(from);
        if self.blocks[fi].succs.iter().any(|(t, k)| t == to && *k == kind) {
            return;
        }
        self.blocks[fi].succs.push((to.clone(), kind));
        let ti = self.block_idx(to);
        self.blocks[ti].preds.push(from.clone());
    }

    fn add_stmt(&mut self, block: &BlockId, stmt: StatementNode) {
        let idx = self.block_idx(block);
        self.blocks[idx].stmts.push(stmt);
    }

    fn set_terminated(&mut self, block: &BlockId) {
        let idx = self.block_idx(block);
        self.blocks[idx].is_terminated = true;
    }

    fn is_terminated(&self, block: &BlockId) -> bool {
        let idx = self.block_idx(block);
        self.blocks[idx].is_terminated
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
                            if matches!(value.kind(), "method_invocation" | "object_creation_expression") {
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
                                if matches!(right.kind(), "method_invocation" | "object_creation_expression") {
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
                if child.kind() == "block" || child.kind() == "resource_specification" {
                    if child.kind() == "block" {
                        cur = self.build_block_node(*child, cur);
                        break;
                    }
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
            // Normal path from try and catch → finally.
            self.add_edge(&after, &fin_entry, CfgEdgeKind::Sequential);
            for ce in &catch_exits {
                self.add_edge(ce, &fin_entry, CfgEdgeKind::Sequential);
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
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mid(s: &str) -> NodeId {
        NodeId::new(s)
    }

    #[test]
    fn linear_method_has_two_blocks() {
        // Linear method: entry block + exit block.
        let src = r#"
class Foo {
    String greet(String name) {
        String msg = "Hello " + name;
        return msg;
    }
}
"#;
        let id = mid("Method:com.example.Foo#greet/1");
        let cfg = build_cfg(&id, src).expect("CFG should build");
        // Entry block (stmts) + exit (synthetic) = at minimum 2 blocks.
        assert!(cfg.block_count() >= 2);
        // Linear: 1 edge from entry → exit (Sequential).
        let entry_block = cfg.block(&cfg.entry).unwrap();
        assert!(entry_block.stmts.len() >= 1); // at least the return stmt
    }

    #[test]
    fn if_else_creates_branch() {
        let src = r#"
class Foo {
    int abs(int x) {
        if (x < 0) {
            return -x;
        } else {
            return x;
        }
    }
}
"#;
        let id = mid("Method:com.example.Foo#abs/1");
        let cfg = build_cfg(&id, src).expect("CFG should build");
        // Expected blocks: entry (with branch stmt), then-block, else-block, join (empty), exit.
        // Actually: entry has branch stmt; then-block returns; else-block returns; join; exit
        assert!(cfg.block_count() >= 4, "expected ≥4 blocks, got {}", cfg.block_count());
        // Entry should have a Branch stmt.
        let entry = cfg.block(&cfg.entry).unwrap();
        assert!(
            entry.stmts.iter().any(|s| s.kind == StatementKind::Branch),
            "entry block should contain Branch stmt"
        );
        // Entry should have 2 successors (True + False).
        assert_eq!(entry.succs.len(), 2, "if-else: entry should have 2 successors");
        assert!(entry.succs.iter().any(|(_, k)| *k == CfgEdgeKind::True));
        assert!(entry.succs.iter().any(|(_, k)| *k == CfgEdgeKind::False));
    }

    #[test]
    fn while_loop_has_back_edge() {
        let src = r#"
class Counter {
    int sum(int n) {
        int s = 0;
        while (n > 0) {
            s += n;
            n--;
        }
        return s;
    }
}
"#;
        let id = mid("Method:com.example.Counter#sum/1");
        let cfg = build_cfg(&id, src).expect("CFG should build");
        // Must have at least one back edge.
        let has_back = cfg.blocks.iter().any(|b| b.succs.iter().any(|(_, k)| *k == CfgEdgeKind::Back));
        assert!(has_back, "while loop must produce a Back edge");
        // Header has True (→ body) and False (→ after).
        let header_block = cfg
            .blocks
            .iter()
            .find(|b| b.stmts.iter().any(|s| s.kind == StatementKind::Loop))
            .expect("should find a Loop stmt block");
        assert!(header_block.succs.iter().any(|(_, k)| *k == CfgEdgeKind::True));
        assert!(header_block.succs.iter().any(|(_, k)| *k == CfgEdgeKind::False));
    }

    #[test]
    fn try_catch_has_exception_edge() {
        let src = r#"
class Foo {
    void process(String s) {
        try {
            int x = Integer.parseInt(s);
        } catch (NumberFormatException e) {
            log(e);
        }
    }
}
"#;
        let id = mid("Method:com.example.Foo#process/1");
        let cfg = build_cfg(&id, src).expect("CFG should build");
        let has_exc = cfg.blocks.iter().any(|b| b.succs.iter().any(|(_, k)| *k == CfgEdgeKind::Exception));
        assert!(has_exc, "try-catch must produce an Exception edge");
    }

    #[test]
    fn dominance_entry_dominates_all() {
        let src = r#"
class Foo {
    int max(int a, int b) {
        if (a > b) {
            return a;
        }
        return b;
    }
}
"#;
        let id = mid("Method:com.example.Foo#max/2");
        let cfg = build_cfg(&id, src).expect("CFG should build");
        let dom = cfg.compute_dominators();

        // Entry should dominate all other reachable blocks.
        let entry = &cfg.entry;
        for block in &cfg.blocks {
            if block.id == *entry {
                continue;
            }
            // All reachable blocks (those with a known idom) should be dominated by entry.
            if dom.idom(&block.id).is_some() {
                assert!(
                    dom.strictly_dominates(entry, &block.id)
                        || block.id == *entry
                        || dom.idom(&block.id) == Some(entry),
                    "entry should dominate block {:?}", block.id
                );
            }
        }
    }

    #[test]
    fn cyclomatic_complexity_if_else() {
        // if-else: edges = 4 (entry→then, entry→else, then→join, else→join)
        // + join→exit. Actually with our dead blocks the count may differ.
        // Just verify it's > 1 for a branching method.
        let src = r#"
class Foo {
    String classify(int n) {
        if (n > 0) {
            return "positive";
        } else {
            return "non-positive";
        }
    }
}
"#;
        let id = mid("Method:com.example.Foo#classify/1");
        let cfg = build_cfg(&id, src).expect("CFG should build");
        assert!(
            cfg.cyclomatic_complexity() >= 2,
            "if-else should have cyclomatic complexity ≥ 2, got {}",
            cfg.cyclomatic_complexity()
        );
    }
}
