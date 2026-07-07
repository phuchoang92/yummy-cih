use cih_core::SymbolDef;
use crate::common::index::CommonIndex;
use super::{InheritanceModel, LanguageResolver};

pub struct GoResolver;

impl LanguageResolver for GoResolver {
    fn language_id(&self) -> &'static str {
        "go"
    }

    fn constructor_name(&self) -> Option<&'static str> {
        None
    }

    fn is_self_receiver(&self, name: &str) -> bool {
        // Go receiver variables are not keywords, but single-letter names are idiomatic
        // We can't reliably detect them without type info, so return false here.
        let _ = name;
        false
    }

    fn resolve_self_receiver(
        &self,
        _keyword: &str,
        _in_fqcn: &str,
        _index: &CommonIndex,
    ) -> Option<String> {
        None
    }

    fn di_redirect(&self, _type_qname: &str, _index: &CommonIndex) -> Option<String> {
        None
    }

    fn type_metadata(&self, _def: &SymbolDef) -> Option<String> {
        None
    }

    fn inheritance_model(&self) -> InheritanceModel {
        InheritanceModel::None
    }
}
