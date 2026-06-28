use std::collections::HashMap;

use crate::common::index::CommonIndex;

/// Compute a C3 linearization for every type in the index.
/// Result: type FQCN → ordered MRO list (self first, then ancestors breadth-first in C3 order).
pub(crate) fn build_mro_map(index: &CommonIndex) -> HashMap<String, Vec<String>> {
    let mut cache: HashMap<String, Vec<String>> = HashMap::new();
    let all: Vec<String> = index.type_fqcns().map(str::to_string).collect();
    for fqcn in &all {
        c3_linearize(fqcn, index, &mut cache);
    }
    cache
}

/// C3 linearization of `fqcn`. Results are memoized in `cache`.
/// Supertypes must be ordered: superclass first (if any), then interfaces — this is guaranteed
/// by [`CommonIndex::dedup`] which uses [`stable_dedup`] and the parse order from java.rs.
pub(crate) fn c3_linearize(
    fqcn: &str,
    index: &CommonIndex,
    cache: &mut HashMap<String, Vec<String>>,
) -> Vec<String> {
    if let Some(cached) = cache.get(fqcn) {
        return cached.clone();
    }
    // Pre-insert sentinel so cycles in the supertype graph don't loop forever.
    cache.insert(fqcn.to_string(), vec![fqcn.to_string()]);

    let bases: Vec<String> = index.supertypes(fqcn).to_vec();
    if bases.is_empty() {
        return vec![fqcn.to_string()];
    }

    // Build the merge input: one linearization per base, plus the bases list itself.
    let mut lists: Vec<Vec<String>> = bases
        .iter()
        .map(|b| c3_linearize(b, index, cache))
        .collect();
    lists.push(bases);

    let mut result = vec![fqcn.to_string()];
    loop {
        lists.retain(|l| !l.is_empty());
        if lists.is_empty() {
            break;
        }
        // Collect tail elements once per iteration — O(n) rather than O(n²) per head.
        let tails: std::collections::HashSet<&String> =
            lists.iter().flat_map(|l| l.iter().skip(1)).collect();
        // Pick the first head that is not in the tail of any list.
        let head = lists
            .iter()
            .find_map(|list| {
                let h = &list[0];
                if !tails.contains(h) { Some(h.clone()) } else { None }
            })
            .unwrap_or_else(|| lists[0][0].clone()); // cycle fallback: take first
        result.push(head.clone());
        // Remove head only from the FRONT of lists that currently start with it.
        // Using retain() would wrongly strip head from tail positions, breaking C3
        // semantics for diamond inheritance.
        for list in &mut lists {
            if list.first() == Some(&head) {
                list.remove(0);
            }
        }
    }

    cache.insert(fqcn.to_string(), result.clone());
    result
}

