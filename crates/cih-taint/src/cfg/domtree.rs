//! Cooper-Harvey-Kennedy immediate-dominator tree and the `DomTree` type.
//!
//! Reference: "A Simple, Fast Dominance Algorithm", Cooper, Harvey, Kennedy (2001).
//! Algorithm runs in O(N · depth) where depth is typically small for well-structured
//! Java methods (no `goto`).

use std::collections::HashMap;

use super::BlockId;

// ── Dominator tree ────────────────────────────────────────────────────────────

/// Immediate-dominator tree for a [`super::Cfg`].
pub struct DomTree {
    /// Maps each reachable block to its immediate dominator.
    /// The entry block maps to itself.
    pub(super) id_to_idom: HashMap<BlockId, BlockId>,
}

impl DomTree {
    pub(super) fn empty() -> Self {
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
                Some(d) if d == cur => return false,
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

// ── Algorithm ─────────────────────────────────────────────────────────────────

/// Compute the immediate-dominator tree for a CFG using Cooper-Harvey-Kennedy (2001).
pub(super) fn compute_dom_tree(cfg: &super::Cfg) -> DomTree {
    let n = cfg.blocks.len();
    if n == 0 {
        return DomTree::empty();
    }

    let rpo = cfg.reverse_post_order();
    let rpo_idx: HashMap<&BlockId, usize> = rpo.iter().enumerate().map(|(i, id)| (id, i)).collect();

    const UNDEF: usize = usize::MAX;
    let mut idom = vec![UNDEF; n];
    let entry_rpo = *rpo_idx.get(&cfg.entry).unwrap_or(&0);
    idom[entry_rpo] = entry_rpo;

    let mut changed = true;
    while changed {
        changed = false;
        for rpo_i in 0..rpo.len() {
            if rpo_i == entry_rpo {
                continue;
            }
            let block_id = &rpo[rpo_i];
            let Some(block) = cfg.block(block_id) else {
                continue;
            };

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

/// Cooper-Harvey-Kennedy `intersect`: walk up both finger-paths until they meet.
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
