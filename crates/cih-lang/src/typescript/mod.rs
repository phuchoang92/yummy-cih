use std::collections::BTreeSet;

use once_cell::sync::Lazy;
use tree_sitter::{Language, Node as TsNode, Query};

use crate::{LanguageProvider, SourceScan, Stereotype};

mod parse;

pub const TS_SCOPE_QUERY: &str = include_str!("query.scm");

static QUERY: Lazy<Query> = Lazy::new(|| {
    Query::new(&language(), TS_SCOPE_QUERY).expect("TypeScript scope query must compile")
});

fn language() -> Language {
    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TypescriptProvider;

impl TypescriptProvider {
    pub fn new() -> Self {
        Self
    }
}

impl LanguageProvider for TypescriptProvider {
    fn language(&self) -> Language {
        language()
    }

    fn language_id(&self) -> &'static str {
        "typescript"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[".ts", ".tsx"]
    }

    fn scope_query(&self) -> &Query {
        &QUERY
    }

    fn package_of(&self, _root: TsNode<'_>, _src: &str) -> Option<String> {
        None
    }

    fn stereotype(&self, def_text: &str) -> Option<Stereotype> {
        if def_text.is_empty() {
            return None;
        }
        if def_text.contains("@Controller") || def_text.contains("@Injectable") {
            return Some(Stereotype::NestJs);
        }
        None
    }

    fn parse_file(&self, rel: &str, src: &str) -> anyhow::Result<cih_core::ParsedUnit> {
        parse::parse_typescript_file(rel, src)
    }

    fn scan_file(&self, _rel: &str, src: &str) -> anyhow::Result<SourceScan> {
        let loc = src.bytes().filter(|b| *b == b'\n').count() as u64;
        let mut frameworks = BTreeSet::new();
        if src.contains("@Controller")
            || src.contains("@Injectable")
            || src.contains("@Module")
        {
            frameworks.insert("nestjs".into());
        }
        Ok(SourceScan {
            loc,
            package: None,
            frameworks,
        })
    }
}


