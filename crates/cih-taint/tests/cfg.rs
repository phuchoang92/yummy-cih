use cih_core::NodeId;
use cih_taint::{build_cfg, CfgEdgeKind, StatementKind};

fn mid(s: &str) -> NodeId {
    NodeId::new(s)
}

#[test]
fn build_cfg_unmatched_method_returns_none() {
    let id = mid("Method:com.example.Foo#noop/0");
    assert!(build_cfg(&id, "class Foo {}").is_none());
}

#[test]
fn linear_method_has_two_blocks() {
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
    assert!(cfg.block_count() >= 2);
    let entry_block = cfg.block(&cfg.entry).unwrap();
    assert!(!entry_block.stmts.is_empty());
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
    assert!(
        cfg.block_count() >= 4,
        "expected ≥4 blocks, got {}",
        cfg.block_count()
    );
    let entry = cfg.block(&cfg.entry).unwrap();
    assert!(
        entry.stmts.iter().any(|s| s.kind == StatementKind::Branch),
        "entry block should contain Branch stmt"
    );
    assert_eq!(
        entry.succs.len(),
        2,
        "if-else: entry should have 2 successors"
    );
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
    let has_back = cfg
        .blocks
        .iter()
        .any(|b| b.succs.iter().any(|(_, k)| *k == CfgEdgeKind::Back));
    assert!(has_back, "while loop must produce a Back edge");
    let header_block = cfg
        .blocks
        .iter()
        .find(|b| b.stmts.iter().any(|s| s.kind == StatementKind::Loop))
        .expect("should find a Loop stmt block");
    assert!(header_block
        .succs
        .iter()
        .any(|(_, k)| *k == CfgEdgeKind::True));
    assert!(header_block
        .succs
        .iter()
        .any(|(_, k)| *k == CfgEdgeKind::False));
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
    let has_exc = cfg
        .blocks
        .iter()
        .any(|b| b.succs.iter().any(|(_, k)| *k == CfgEdgeKind::Exception));
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

    let entry = &cfg.entry;
    for block in &cfg.blocks {
        if block.id == *entry {
            continue;
        }
        if dom.idom(&block.id).is_some() {
            assert!(
                dom.strictly_dominates(entry, &block.id)
                    || block.id == *entry
                    || dom.idom(&block.id) == Some(entry),
                "entry should dominate block {:?}",
                block.id
            );
        }
    }
}

#[test]
fn cyclomatic_complexity_if_else() {
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
