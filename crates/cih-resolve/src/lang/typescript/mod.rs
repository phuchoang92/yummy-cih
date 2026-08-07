use crate::index::ResolveIndex;
use crate::lang::{InheritanceModel, LanguageResolver};
use crate::types::class_of;

pub struct TypeScriptResolver;

impl LanguageResolver for TypeScriptResolver {
    fn language_id(&self) -> &'static str {
        "typescript"
    }

    fn constructor_name(&self) -> Option<&'static str> {
        Some("constructor")
    }

    fn is_self_receiver(&self, name: &str) -> bool {
        name == "this"
    }

    fn resolve_self_receiver(
        &self,
        keyword: &str,
        in_fqcn: &str,
        _index: &ResolveIndex,
    ) -> Option<String> {
        if keyword == "this" {
            Some(class_of(in_fqcn).to_string())
        } else {
            None
        }
    }

    fn inheritance_model(&self) -> InheritanceModel {
        InheritanceModel::TypeScriptNominal
    }
}
