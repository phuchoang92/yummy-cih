use cih_core::SymbolDef;
use crate::common::index::CommonIndex;
use super::{InheritanceModel, LanguageResolver};

pub struct PhpResolver;

impl LanguageResolver for PhpResolver {
    fn language_id(&self) -> &'static str {
        "php"
    }

    fn constructor_name(&self) -> Option<&'static str> {
        Some("__construct")
    }

    fn is_self_receiver(&self, name: &str) -> bool {
        name == "$this" || name == "self" || name == "static" || name == "parent"
    }

    fn resolve_self_receiver(
        &self,
        _keyword: &str,
        in_fqcn: &str,
        _index: &CommonIndex,
    ) -> Option<String> {
        in_fqcn.rsplitn(2, '.').nth(1).map(str::to_string)
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
