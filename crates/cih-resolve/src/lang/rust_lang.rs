use cih_core::SymbolDef;
use crate::common::index::CommonIndex;
use super::{InheritanceModel, LanguageResolver};

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
        _index: &CommonIndex,
    ) -> Option<String> {
        // in_fqcn for Rust methods is "ModulePath::TypeName::method_name"
        // The owner type is the second-to-last segment
        let parts: Vec<&str> = in_fqcn.rsplitn(2, "::").collect();
        parts.get(1).map(|s| s.to_string())
    }

    fn inheritance_model(&self) -> InheritanceModel {
        InheritanceModel::None
    }

    fn type_metadata(&self, _def: &SymbolDef) -> Option<String> {
        None
    }

    fn di_redirect(&self, _type_qname: &str, _index: &CommonIndex) -> Option<String> {
        None
    }
}
