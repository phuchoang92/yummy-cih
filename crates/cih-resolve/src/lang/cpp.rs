use super::{InheritanceModel, LanguageResolver};
use crate::index::CommonIndex;
use cih_core::SymbolDef;

pub struct CppResolver;

impl LanguageResolver for CppResolver {
    fn language_id(&self) -> &'static str {
        "cpp"
    }

    fn constructor_name(&self) -> Option<&'static str> {
        None
    }

    fn is_self_receiver(&self, name: &str) -> bool {
        name == "this"
    }

    fn resolve_self_receiver(
        &self,
        _keyword: &str,
        in_fqcn: &str,
        _index: &CommonIndex,
    ) -> Option<String> {
        // C++ FQCN: "ClassName::method_name"
        in_fqcn.rsplit_once("::").map(|(owner, _)| owner.to_string())
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
