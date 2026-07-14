use crate::index::ResolveIndex;
use crate::lang::{InheritanceModel, LanguageResolver};
use crate::types::class_of;

pub struct KotlinResolver;

impl LanguageResolver for KotlinResolver {
    fn language_id(&self) -> &'static str {
        "kotlin"
    }

    fn constructor_name(&self) -> Option<&'static str> {
        Some("<init>")
    }

    fn is_self_receiver(&self, name: &str) -> bool {
        name == "this" || name == "super"
    }

    fn resolve_self_receiver(
        &self,
        keyword: &str,
        in_fqcn: &str,
        _index: &ResolveIndex,
    ) -> Option<String> {
        match keyword {
            "this" => Some(class_of(in_fqcn).to_string()),
            "super" => Some(class_of(in_fqcn).to_string()),
            _ => None,
        }
    }

    fn inheritance_model(&self) -> InheritanceModel {
        InheritanceModel::JavaNominal
    }
}
