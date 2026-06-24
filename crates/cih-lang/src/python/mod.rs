use once_cell::sync::Lazy;
use tree_sitter::{Language, Node as TsNode, Query};

use crate::{LanguageProvider, Stereotype};

pub mod parse;

pub const PY_SCOPE_QUERY: &str = include_str!("query.scm");

static QUERY: Lazy<Query> = Lazy::new(|| {
    Query::new(&language(), PY_SCOPE_QUERY).expect("Python scope query must compile")
});

fn language() -> Language {
    tree_sitter_python::LANGUAGE.into()
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PythonProvider;

impl PythonProvider {
    pub fn new() -> Self {
        Self
    }
}

impl LanguageProvider for PythonProvider {
    fn language(&self) -> Language {
        language()
    }

    fn language_id(&self) -> &'static str {
        "python"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[".py"]
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
        if def_text.contains("@app.route")
            || def_text.contains("@app.get")
            || def_text.contains("@app.post")
            || def_text.contains("@blueprint")
        {
            return Some(Stereotype::Flask);
        }
        if def_text.contains("@router.get")
            || def_text.contains("@router.post")
            || def_text.contains("@router.put")
            || def_text.contains("@router.delete")
        {
            return Some(Stereotype::FastApi);
        }
        None
    }

    fn parse_file(&self, rel: &str, src: &str) -> anyhow::Result<cih_core::ParsedUnit> {
        parse::parse_python_file(rel, src)
    }
}

#[cfg(test)]
mod tests;

