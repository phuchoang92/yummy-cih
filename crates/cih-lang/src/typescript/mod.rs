use once_cell::sync::Lazy;
use tree_sitter::{Language, Node as TsNode, Query};

use crate::{LanguageProvider, Stereotype};

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::NodeKind;

    const NESTJS_SAMPLE: &str = r#"
import { Controller, Get, Post } from '@nestjs/common';

@Controller('orders')
export class OrderController {
    @Get(':id')
    findOne(id: string) {
        return null;
    }

    @Post()
    create(body: any) {
        return null;
    }
}
"#;

    #[test]
    fn nestjs_routes_extracted_with_correct_path() {
        let provider = TypescriptProvider::new();
        let unit = provider
            .parse_file("src/orders/order.controller.ts", NESTJS_SAMPLE)
            .expect("should parse");
        let routes: Vec<_> = unit
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Route)
            .collect();
        let names: Vec<&str> = routes.iter().map(|n| n.name.as_str()).collect();
        assert!(
            names.iter().any(|n| n.contains("GET")),
            "expected a GET route, got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("POST")),
            "expected a POST route, got: {names:?}"
        );
        // Props should include source = nestjs
        for route in &routes {
            let props = route.props.as_ref().expect("route has props");
            assert!(
                props["source"].as_str().unwrap_or("").contains("nest"),
                "source should be nest_js, got: {props}"
            );
        }
    }

    const PLAIN_SAMPLE: &str = r#"
export class UserService {
    findAll() { return []; }
    findOne(id: string) { return null; }
}

export function greet(name: string): string {
    return `Hello ${name}`;
}
"#;

    #[test]
    fn plain_class_and_function_nodes_emitted() {
        let provider = TypescriptProvider::new();
        let unit = provider
            .parse_file("src/user.service.ts", PLAIN_SAMPLE)
            .expect("should parse");

        let class_nodes: Vec<_> = unit
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Class)
            .collect();
        assert!(!class_nodes.is_empty(), "expected a Class node");
        assert!(
            class_nodes.iter().any(|n| n.name == "UserService"),
            "expected UserService class"
        );

        let fn_nodes: Vec<_> = unit
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Function)
            .collect();
        assert!(!fn_nodes.is_empty(), "expected Function nodes");
        assert!(
            fn_nodes.iter().any(|n| n.name == "greet"),
            "expected greet function"
        );
    }

    const EXPRESS_SAMPLE: &str = r#"
const express = require('express');
const app = express();

app.get('/users', (req, res) => {
    res.json([]);
});

app.post('/users', (req, res) => {
    res.status(201).json({});
});
"#;

    #[test]
    fn express_router_routes_emitted() {
        let provider = TypescriptProvider::new();
        let unit = provider
            .parse_file("src/server.ts", EXPRESS_SAMPLE)
            .expect("should parse");
        let routes: Vec<_> = unit
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Route)
            .collect();
        assert!(!routes.is_empty(), "expected Express route nodes");
        let names: Vec<&str> = routes.iter().map(|n| n.name.as_str()).collect();
        assert!(
            names.iter().any(|n| n.contains("GET")),
            "expected GET /users, got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("POST")),
            "expected POST /users, got: {names:?}"
        );
    }
}
