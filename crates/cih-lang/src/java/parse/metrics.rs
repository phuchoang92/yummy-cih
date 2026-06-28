use cih_core::ComplexityRecord;
use crate::fingerprint::{compute_body_fingerprint, normalize_leaf_token_java};
use tree_sitter::Node as TsNode;

pub(super) fn compute_complexity(body: TsNode<'_>) -> ComplexityRecord {
    let mut cyclomatic: u16 = 1; // base
    let mut cognitive: u16 = 0;
    let mut loop_depth: u8 = 0;
    let mut counts = ControlFlowCounts::default();
    compute_complexity_inner(body, 0, 0, &mut cyclomatic, &mut cognitive, &mut loop_depth, &mut counts);
    ComplexityRecord {
        provider: "java".to_string(),
        cyclomatic,
        cognitive,
        loop_depth,
        is_recursive: false,
        if_count: counts.if_count,
        for_count: counts.for_count,
        while_count: counts.while_count,
        switch_count: counts.switch_count,
        try_count: counts.try_count,
        return_count: counts.return_count,
        throw_count: counts.throw_count,
    }
}

#[derive(Default)]
struct ControlFlowCounts {
    if_count: u16,
    for_count: u16,
    while_count: u16,
    switch_count: u16,
    try_count: u16,
    return_count: u16,
    throw_count: u16,
}

#[allow(clippy::too_many_arguments)]
fn compute_complexity_inner(
    node: TsNode<'_>,
    nesting: u16,
    loop_nesting: u8,
    cyclomatic: &mut u16,
    cognitive: &mut u16,
    max_loop_depth: &mut u8,
    counts: &mut ControlFlowCounts,
) {
    let kind = node.kind();

    let (new_nesting, new_loop_nesting) = match kind {
        "if_statement" => {
            *cyclomatic = cyclomatic.saturating_add(1);
            *cognitive = cognitive.saturating_add(1 + nesting);
            counts.if_count = counts.if_count.saturating_add(1);
            (nesting + 1, loop_nesting)
        }
        "while_statement" | "do_statement" => {
            *cyclomatic = cyclomatic.saturating_add(1);
            *cognitive = cognitive.saturating_add(1 + nesting);
            counts.while_count = counts.while_count.saturating_add(1);
            let new_ld = loop_nesting + 1;
            if new_ld > *max_loop_depth {
                *max_loop_depth = new_ld;
            }
            (nesting + 1, new_ld)
        }
        "for_statement" | "enhanced_for_statement" => {
            *cyclomatic = cyclomatic.saturating_add(1);
            *cognitive = cognitive.saturating_add(1 + nesting);
            counts.for_count = counts.for_count.saturating_add(1);
            let new_ld = loop_nesting + 1;
            if new_ld > *max_loop_depth {
                *max_loop_depth = new_ld;
            }
            (nesting + 1, new_ld)
        }
        "switch_expression" | "switch_statement" => {
            *cognitive = cognitive.saturating_add(1 + nesting);
            counts.switch_count = counts.switch_count.saturating_add(1);
            (nesting + 1, loop_nesting)
        }
        "switch_label" => {
            *cyclomatic = cyclomatic.saturating_add(1);
            (nesting, loop_nesting)
        }
        "catch_clause" => {
            *cyclomatic = cyclomatic.saturating_add(1);
            *cognitive = cognitive.saturating_add(1 + nesting);
            (nesting + 1, loop_nesting)
        }
        "try_statement" => {
            *cognitive = cognitive.saturating_add(1 + nesting);
            counts.try_count = counts.try_count.saturating_add(1);
            (nesting + 1, loop_nesting)
        }
        "return_statement" => {
            counts.return_count = counts.return_count.saturating_add(1);
            (nesting, loop_nesting)
        }
        "throw_statement" => {
            counts.throw_count = counts.throw_count.saturating_add(1);
            (nesting, loop_nesting)
        }
        "conditional_expression" => {
            *cyclomatic = cyclomatic.saturating_add(1);
            *cognitive = cognitive.saturating_add(1 + nesting);
            (nesting, loop_nesting)
        }
        "binary_expression" => {
            (nesting, loop_nesting)
        }
        "else" => {
            *cognitive = cognitive.saturating_add(1);
            (nesting, loop_nesting)
        }
        "break_statement" | "continue_statement" => {
            if node.child_by_field_name("label").is_some() {
                *cognitive = cognitive.saturating_add(1);
            }
            (nesting, loop_nesting)
        }
        "lambda_expression" => {
            (0, loop_nesting)
        }
        _ => (nesting, loop_nesting),
    };

    if kind == "binary_expression" {
        if let Some(op) = node.child_by_field_name("operator") {
            let op_text = op.kind();
            if op_text == "&&" || op_text == "||" {
                *cyclomatic = cyclomatic.saturating_add(1);
                *cognitive = cognitive.saturating_add(1);
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        compute_complexity_inner(
            child,
            new_nesting,
            new_loop_nesting,
            cyclomatic,
            cognitive,
            max_loop_depth,
            counts,
        );
    }
}

pub(super) fn java_body_fingerprint(body: TsNode<'_>) -> Option<cih_core::BodyFingerprint> {
    compute_body_fingerprint(body, "java", normalize_leaf_token_java)
}
