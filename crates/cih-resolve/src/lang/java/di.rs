use crate::index::ResolveIndex;

const SPRING_BEANS: &[&str] = &[
    "service",
    "repository",
    "component",
    "controller",
    "configuration",
];

pub(crate) fn is_spring_bean(fqcn: &str, index: &ResolveIndex) -> bool {
    matches!(
        index.type_metadata_for(fqcn),
        Some(s) if SPRING_BEANS.contains(&s)
    )
}

/// Resolve a `@Qualifier("id")` / `@Resource(name = "id")` against the DI XML bean
/// registry: the named bean's class, when it is in scope and actually implements
/// (or extends, transitively) the declared interface type.
pub(crate) fn qualifier_bean_impl(
    interface_fqcn: &str,
    qualifier: &str,
    index: &ResolveIndex,
) -> Option<String> {
    let fqcn = index.bean_class_by_id(qualifier)?;
    if !index.is_known_type(fqcn) {
        return None;
    }
    if !implements_transitively(fqcn, interface_fqcn, index) {
        return None;
    }
    Some(fqcn.to_string())
}

/// True when `interface_fqcn` appears in `fqcn`'s supertype closure. Supertype
/// entries whose import resolution failed are stored unqualified, so those also
/// match on simple name (common across OSGi bundle boundaries).
fn implements_transitively(fqcn: &str, interface_fqcn: &str, index: &ResolveIndex) -> bool {
    let interface_simple = crate::di_xml::simple_name(interface_fqcn);
    let mut visited = std::collections::HashSet::new();
    let mut queue: std::collections::VecDeque<&str> = index
        .supertypes(fqcn)
        .iter()
        .map(String::as_str)
        .collect();
    while let Some(super_fqcn) = queue.pop_front() {
        if !visited.insert(super_fqcn) {
            continue;
        }
        if super_fqcn == interface_fqcn
            || (!super_fqcn.contains('.') && super_fqcn == interface_simple)
        {
            return true;
        }
        queue.extend(index.supertypes(super_fqcn).iter().map(String::as_str));
    }
    false
}

/// Returns the single @Service/@Component/@Repository implementor of `interface_fqcn`,
/// or None when there are zero or multiple (ambiguous).
///
/// BFS through the implementors graph so that a concrete `@Service` that extends an
/// abstract intermediary class (which directly implements the interface) is found even
/// though it is not a direct implementor of the interface.
pub(crate) fn single_bean_impl(interface_fqcn: &str, index: &ResolveIndex) -> Option<String> {
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
