use cih_core::{ImportBinding, ImportBindingKind};

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

    fn resolve_import(
        &self,
        binding: &ImportBinding,
        from_file: &str,
        index: &ResolveIndex,
    ) -> Option<String> {
        match binding.kind {
            ImportBindingKind::Named => {
                if let Some(imported) = &binding.imported {
                    // `from orders.service import OrderService`
                    let candidate = format!("{}.{}", binding.module, imported);
                    if index.is_known_type(&candidate) {
                        return Some(candidate);
                    }
                    index.resolve_type_in_language(imported, from_file, "python")
                } else {
                    None
                }
            }
            ImportBindingKind::Module => None, // `import orders.service` — the module itself
            _ => None,
        }
    }
}
