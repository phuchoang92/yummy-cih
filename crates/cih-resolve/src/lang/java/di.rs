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
pub(crate) fn single_bean_impl(interface_fqcn: &str, index: &CommonIndex) -> Option<String> {
    let impls = index.implementors(interface_fqcn);
    let beans: Vec<&String> = impls
        .iter()
        .filter(|f| is_spring_bean(f, index))
        .collect();
    if beans.len() == 1 {
        Some(beans[0].clone())
    } else {
        None
    }
}
