use super::{InheritanceModel, LanguageResolver};
use crate::index::ResolveIndex;
use crate::types::container_of_callable;
use cih_core::SymbolDef;

pub struct RustResolver;

impl LanguageResolver for RustResolver {
    fn language_id(&self) -> &'static str {
        "rust"
    }

    fn constructor_name(&self) -> Option<&'static str> {
        Some("new")
    }

    fn is_self_receiver(&self, name: &str) -> bool {
        name == "self" || name == "Self"
    }

    fn resolve_self_receiver(
        &self,
        _keyword: &str,
        in_fqcn: &str,
        _index: &ResolveIndex,
    ) -> Option<String> {
        // Rust parser scopes call sites as `container#name/arity`, matching the
        // rest of the IR and preserving `::` inside the container itself.
        Some(container_of_callable(in_fqcn).to_string())
    }

    fn type_metadata(&self, _def: &SymbolDef) -> Option<String> {
        None
    }

    fn inheritance_model(&self) -> InheritanceModel {
        InheritanceModel::None
    }
}
