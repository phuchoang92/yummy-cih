use std::collections::BTreeSet;

use once_cell::sync::Lazy;
use tree_sitter::{Language, Node as TsNode, Query};

use crate::{LanguageProvider, SourceScan, Stereotype};

mod builder;
mod helpers;
mod components;
mod db;
mod file_routes;
mod http_clients;
mod messaging;
mod routes;
mod parse;

pub const TS_SCOPE_QUERY: &str = include_str!("query.scm");

/// Tree-sitter kinds that represent a callable in TS/JS — the *denominator* of the
/// extraction-coverage ratio (see `LanguageProvider::callable_kinds`). Every kind
/// here is something a reader would call "a function"; if one of them routinely
/// fails to become a `Function`/`Method` node, we have a silent parser gap.
pub(super) const CALLABLE_KINDS: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "function_expression",
    "function",
    "arrow_function",
    "method_definition",
];

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

    /// TypeScript **and** JavaScript. JS is parsed with the TypeScript grammar
    /// (a syntactic superset), so `.js`/`.mjs`/`.cjs` parse cleanly and `.jsx`
    /// gets the same error-tolerant JSX handling as `.tsx`.
    fn extensions(&self) -> &'static [&'static str] {
        &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"]
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

    fn callable_kinds(&self) -> &'static [&'static str] {
        CALLABLE_KINDS
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
        // Express (common in JavaScript): `require('express')` / `from 'express'`.
        if src.contains("require('express')")
            || src.contains("require(\"express\")")
            || src.contains("from 'express'")
            || src.contains("from \"express\"")
        {
            frameworks.insert("express".into());
        }
        // Additional backend frameworks — cheap import string-match (single or
        // double quotes), matching the import-gating in the parser.
        for (needle, fw) in [
            ("fastify", "fastify"),
            ("@koa/router", "koa"),
            ("@hapi/hapi", "hapi"),
            ("next", "nextjs"),
            ("@remix-run/", "remix"),
            ("@trpc/server", "trpc"),
            ("type-graphql", "graphql"),
            ("@nestjs/graphql", "graphql"),
        ] {
            if src.contains(&format!("'{needle}"))
                || src.contains(&format!("\"{needle}"))
            {
                frameworks.insert(fw.into());
            }
        }
        Ok(SourceScan {
            loc,
            package: None,
            frameworks,
        })
    }
}


