use cih_core::ImportBinding;

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

    fn resolve_import(
        &self,
        binding: &ImportBinding,
        from_file: &str,
        index: &ResolveIndex,
    ) -> Option<String> {
        use cih_core::ImportBindingKind;
        // Kotlin imports are fully qualified — try direct lookup then simple-name fallback
        let module = &binding.module;
        match binding.kind {
            ImportBindingKind::Named | ImportBindingKind::StaticMember => {
                if index.is_known_type(module) {
                    return Some(module.clone());
                }
                // Strip last segment as simple name, try workspace lookup
                let simple = module.rsplit('.').next().unwrap_or(module.as_str());
                index.resolve_type_in_language(simple, from_file, "kotlin")
            }
            ImportBindingKind::Wildcard => None,
            _ => None,
        }
    }
}
