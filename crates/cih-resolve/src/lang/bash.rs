use super::{InheritanceModel, LanguageResolver};
use crate::index::CommonIndex;
use cih_core::SymbolDef;

pub struct BashResolver;

impl LanguageResolver for BashResolver {
    fn language_id(&self) -> &'static str {
        "bash"
    }

    fn constructor_name(&self) -> Option<&'static str> {
        None
    }

    fn is_self_receiver(&self, _name: &str) -> bool {
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
