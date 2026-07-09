use cih_core::{ImportBinding, ImportBindingKind};

use crate::index::CommonIndex;
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
        _index: &CommonIndex,
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

    fn resolve_import(
        &self,
        binding: &ImportBinding,
        from_file: &str,
        index: &CommonIndex,
    ) -> Option<String> {
        let module = &binding.module;
        if !module.starts_with('.') && !module.starts_with('/') {
            return None; // external package — not in workspace
        }
        // Build directory from the source file path
        let dir = {
            let parts: Vec<&str> = from_file.rsplitn(2, '/').collect();
            if parts.len() == 2 {
                parts[1]
            } else {
                ""
            }
        };
        let resolved_path = resolve_relative(dir, module);
        match binding.kind {
            ImportBindingKind::Named => {
                if let Some(imported) = &binding.imported {
                    let dotted = resolved_path.replace('/', ".");
                    let candidate = format!("{}.{}", dotted.trim_matches('.'), imported);
                    if index.is_known_type(&candidate) {
                        return Some(candidate);
                    }
                    // Fallback: unique simple name in typescript language
                    index.resolve_type_in_language(imported, from_file, "typescript")
                } else {
                    None
                }
            }
            ImportBindingKind::Default => index.resolve_type_in_language(
                binding.local.as_deref().unwrap_or(""),
                from_file,
                "typescript",
            ),
            ImportBindingKind::Wildcard
            | ImportBindingKind::Namespace
            | ImportBindingKind::Module
            | ImportBindingKind::StaticMember => None,
        }
    }
}

fn resolve_relative(dir: &str, module: &str) -> String {
    // Simple relative path resolution: join dir + module, normalize ..
    let base = if dir.is_empty() {
        module.to_string()
    } else {
        format!("{}/{}", dir, module)
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in base.split('/') {
        match seg {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            s => parts.push(s),
        }
    }
    parts.join("/")
}
