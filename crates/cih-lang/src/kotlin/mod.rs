use std::collections::BTreeSet;

use once_cell::sync::Lazy;
use tree_sitter::{Language, Node as TsNode, Query};

use crate::{LanguageProvider, SourceScan, Stereotype};

mod parse;

pub const KT_SCOPE_QUERY: &str = include_str!("query.scm");

static QUERY: Lazy<Query> = Lazy::new(|| {
    Query::new(&language(), KT_SCOPE_QUERY).expect("Kotlin scope query must compile")
});

fn language() -> Language {
    tree_sitter_kotlin_updated::language()
}

#[derive(Clone, Copy, Debug, Default)]
pub struct KotlinProvider;

impl KotlinProvider {
    pub fn new() -> Self {
        Self
    }
}

impl LanguageProvider for KotlinProvider {
    fn language(&self) -> Language {
        language()
    }

    fn language_id(&self) -> &'static str {
        "kotlin"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[".kt", ".kts"]
    }

    fn scope_query(&self) -> &Query {
        &QUERY
    }

    fn package_of(&self, root: TsNode<'_>, src: &str) -> Option<String> {
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            if child.kind() == "package_header" {
                let mut ic = child.walk();
                for pkg_child in child.named_children(&mut ic) {
                    if pkg_child.kind() == "identifier" {
                        return Some(
                            pkg_child
                                .utf8_text(src.as_bytes())
                                .unwrap_or_default()
                                .trim()
                                .to_string(),
                        );
                    }
                }
            }
        }
        None
    }

    fn stereotype(&self, def_text: &str) -> Option<Stereotype> {
        if def_text.is_empty() {
            return None;
        }
        if def_text.contains("@RestController")
            || def_text.contains("@Controller")
            || def_text.contains("@RequestMapping")
            || def_text.contains("@GetMapping")
            || def_text.contains("@PostMapping")
        {
            return Some(Stereotype::Spring);
        }
        None
    }

    fn parse_file(&self, rel: &str, src: &str) -> anyhow::Result<cih_core::ParsedUnit> {
        parse::parse_kotlin_file(rel, src)
    }

    fn scan_file(&self, _rel: &str, src: &str) -> anyhow::Result<SourceScan> {
        let loc = src.bytes().filter(|b| *b == b'\n').count() as u64;
        let package = scan_extract_package(src);
        let mut frameworks = BTreeSet::new();
        if has_spring_signal(src) {
            frameworks.insert("spring".into());
        }
        Ok(SourceScan {
            loc,
            package,
            frameworks,
        })
    }
}

fn scan_extract_package(src: &str) -> Option<String> {
    for line in src.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("package ") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn has_spring_signal(src: &str) -> bool {
    const SPRING_MARKERS: &[&str] = &[
        "@RestController",
        "@Controller",
        "@Service",
        "@Repository",
        "@Component",
        "@Configuration",
        "@Entity",
        "@RequestMapping",
        "@GetMapping",
        "@PostMapping",
        "@PutMapping",
        "@PatchMapping",
        "@DeleteMapping",
    ];
    SPRING_MARKERS.iter().any(|m| src.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kotlin_parse_basic() {
        let provider = KotlinProvider::new();
        let src = r#"package com.example.service

import com.example.model.User
import com.example.repo.*

class UserService(
    private val userRepo: String
) {
    fun findUser(id: Long): String {
        return ""
    }
}

interface UserRepository {
    fun findById(id: Long): String
}

object UserCache {
    fun get(id: Long): String = ""
}
"#;
        let unit = provider.parse_file("src/main/kotlin/UserService.kt", src).unwrap();
        assert_eq!(unit.parsed_file.package.as_deref(), Some("com.example.service"));
        assert_eq!(unit.parsed_file.imports.len(), 2);
        assert!(!unit.parsed_file.imports[0].is_wildcard);
        assert!(unit.parsed_file.imports[1].is_wildcard);
        // Should have Class (UserService), Interface (UserRepository), Class (UserCache)
        let class_nodes: Vec<_> = unit.nodes.iter()
            .filter(|n| matches!(n.kind, cih_core::NodeKind::Class | cih_core::NodeKind::Interface))
            .collect();
        assert!(class_nodes.len() >= 3, "expected >=3 type nodes, got {}", class_nodes.len());
        // Methods
        let method_nodes: Vec<_> = unit.nodes.iter()
            .filter(|n| matches!(n.kind, cih_core::NodeKind::Method | cih_core::NodeKind::Function))
            .collect();
        assert!(method_nodes.len() >= 2, "expected >=2 method nodes, got {}", method_nodes.len());
    }
}
