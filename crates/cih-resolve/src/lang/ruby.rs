use cih_core::SymbolDef;
use crate::common::index::CommonIndex;
use super::{InheritanceModel, LanguageResolver};

pub struct RubyResolver;

impl LanguageResolver for RubyResolver {
    fn language_id(&self) -> &'static str {
        "ruby"
    }

    fn constructor_name(&self) -> Option<&'static str> {
        Some("initialize")
    }

    fn is_self_receiver(&self, name: &str) -> bool {
        name == "self"
    }

    fn resolve_self_receiver(
        &self,
        _keyword: &str,
        in_fqcn: &str,
        _index: &CommonIndex,
    ) -> Option<String> {
        // Ruby FQCN: "ClassName.method_name" — strip method
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
