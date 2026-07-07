use crate::common::index::CommonIndex;

const SPRING_BEANS: &[&str] = &[
    "service",
    "repository",
    "component",
    "controller",
    "configuration",
];

pub(crate) fn is_spring_bean(fqcn: &str, index: &CommonIndex) -> bool {
    matches!(
        index.type_metadata_for(fqcn),
        Some(s) if SPRING_BEANS.contains(&s)
    )
}

/// Returns the single @Service/@Component/@Repository implementor of `interface_fqcn`,
/// or None when there are zero or multiple (ambiguous).
///
/// BFS through the implementors graph so that a concrete `@Service` that extends an
/// abstract intermediary class (which directly implements the interface) is found even
/// though it is not a direct implementor of the interface.
pub(crate) fn single_bean_impl(interface_fqcn: &str, index: &CommonIndex) -> Option<String> {
    let mut visited = std::collections::HashSet::new();
    let mut queue: std::collections::VecDeque<String> =
        index.implementors(interface_fqcn).iter().cloned().collect();
    let mut beans: Vec<String> = Vec::new();
    while let Some(fqcn) = queue.pop_front() {
        if !visited.insert(fqcn.clone()) {
            continue;
        }
        if is_spring_bean(&fqcn, index) {
            beans.push(fqcn);
        } else {
            // fqcn is (likely) an abstract class — walk its subclasses via implementors.
            for sub in index.implementors(&fqcn) {
                queue.push_back(sub.clone());
            }
        }
    }
    if beans.len() == 1 {
        Some(beans[0].clone())
    } else {
        None
    }
}
