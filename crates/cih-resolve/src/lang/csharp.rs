use cih_core::SymbolDef;
use crate::common::index::CommonIndex;
use super::{InheritanceModel, LanguageResolver};

pub struct CSharpResolver;

impl LanguageResolver for CSharpResolver {
    fn language_id(&self) -> &'static str {
        "csharp"
    }

    fn constructor_name(&self) -> Option<&'static str> {
        None // Constructors use the class name in C#
    }

    fn is_self_receiver(&self, name: &str) -> bool {
        name == "this" || name == "base"
    }

    fn resolve_self_receiver(
        &self,
        keyword: &str,
        in_fqcn: &str,
        index: &CommonIndex,
    ) -> Option<String> {
        if keyword == "this" {
            // in_fqcn is "Namespace.ClassName.MethodName" — strip last segment
            let (owner, _) = in_fqcn.rsplit_once('.')?;
            return Some(owner.to_string());
        }
        if keyword == "base" {
            let (owner, _) = in_fqcn.rsplit_once('.')?;
            // Return the first supertype if known
            let supers = index.supertypes(owner);
            return supers.first().cloned();
        }
        None
    }

    fn di_redirect(&self, _type_qname: &str, _index: &CommonIndex) -> Option<String> {
        None
    }

    fn type_metadata(&self, _def: &SymbolDef) -> Option<String> {
        None
    }

    fn inheritance_model(&self) -> InheritanceModel {
        InheritanceModel::TypeScriptNominal
    }
}
