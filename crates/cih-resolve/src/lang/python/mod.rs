use crate::index::ResolveIndex;
use crate::lang::{InheritanceModel, LanguageResolver};
use crate::types::class_of;

pub struct PythonResolver;

impl LanguageResolver for PythonResolver {
    fn language_id(&self) -> &'static str {
        "python"
    }

    fn constructor_name(&self) -> Option<&'static str> {
        Some("__init__")
    }

    fn is_self_receiver(&self, name: &str) -> bool {
        matches!(name, "self" | "cls")
    }

    fn resolve_self_receiver(
        &self,
        _keyword: &str,
        in_fqcn: &str,
        _index: &ResolveIndex,
    ) -> Option<String> {
        // self/cls resolve to the enclosing class
        Some(class_of(in_fqcn).to_string())
    }

    fn inheritance_model(&self) -> InheritanceModel {
        InheritanceModel::PythonC3
    }
}
