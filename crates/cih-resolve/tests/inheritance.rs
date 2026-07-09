// Tests for C3 linearization semantics using a local reimplementation.
// These are self-contained — no cih_resolve internals needed.

fn mro(fqcn: &str, supertypes: &[(&str, &[&str])]) -> Vec<String> {
    let map: std::collections::HashMap<String, Vec<String>> = supertypes
        .iter()
        .map(|(k, vs)| (k.to_string(), vs.iter().map(|v| v.to_string()).collect()))
        .collect();
    linearize_from_map(fqcn, &map)
}

fn linearize_from_map(
    fqcn: &str,
    map: &std::collections::HashMap<String, Vec<String>>,
) -> Vec<String> {
    let mut cache = std::collections::HashMap::new();
    linearize_rec(fqcn, map, &mut cache)
}

fn linearize_rec(
    fqcn: &str,
    map: &std::collections::HashMap<String, Vec<String>>,
    cache: &mut std::collections::HashMap<String, Vec<String>>,
) -> Vec<String> {
    if let Some(c) = cache.get(fqcn) {
        return c.clone();
    }
    cache.insert(fqcn.to_string(), vec![fqcn.to_string()]);
    let bases = map.get(fqcn).cloned().unwrap_or_default();
    if bases.is_empty() {
        return vec![fqcn.to_string()];
    }
    let mut lists: Vec<Vec<String>> = bases.iter().map(|b| linearize_rec(b, map, cache)).collect();
    lists.push(bases.clone());
    let mut result = vec![fqcn.to_string()];
    loop {
        lists.retain(|l| !l.is_empty());
        if lists.is_empty() {
            break;
        }
        let tails: std::collections::HashSet<&String> =
            lists.iter().flat_map(|l| l.iter().skip(1)).collect();
        let head = lists
            .iter()
            .find_map(|list| {
                let h = &list[0];
                if !tails.contains(h) {
                    Some(h.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| lists[0][0].clone());
        result.push(head.clone());
        for list in &mut lists {
            if list.first() == Some(&head) {
                list.remove(0);
            }
        }
    }
    cache.insert(fqcn.to_string(), result.clone());
    result
}

/// Diamond: D extends B, C; B extends A; C extends A.
/// Correct C3 order: [D, B, C, A].
#[test]
fn c3_diamond_inheritance() {
    let result = mro("D", &[("D", &["B", "C"]), ("B", &["A"]), ("C", &["A"])]);
    assert_eq!(
        result,
        vec!["D", "B", "C", "A"],
        "diamond C3 must be [D, B, C, A]; got {result:?}"
    );
}

/// Simple linear: D extends C extends B extends A.
#[test]
fn c3_linear_chain() {
    let result = mro("D", &[("D", &["C"]), ("C", &["B"]), ("B", &["A"])]);
    assert_eq!(result, vec!["D", "C", "B", "A"]);
}

/// No supertypes: result is just the type itself.
#[test]
fn c3_no_supertypes() {
    let result = mro("A", &[]);
    assert_eq!(result, vec!["A"]);
}
