use cih_lang::{kotlin::KotlinProvider, LanguageProvider};

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
    let unit = provider
        .parse_file("src/main/kotlin/UserService.kt", src)
        .unwrap();
    assert_eq!(
        unit.parsed_file.package.as_deref(),
        Some("com.example.service")
    );
    assert_eq!(unit.parsed_file.imports.len(), 2);
    assert!(!unit.parsed_file.imports[0].is_wildcard);
    assert!(unit.parsed_file.imports[1].is_wildcard);
    let class_nodes: Vec<_> = unit
        .nodes
        .iter()
        .filter(|n| {
            matches!(
                n.kind,
                cih_core::NodeKind::Class | cih_core::NodeKind::Interface
            )
        })
        .collect();
    assert!(
        class_nodes.len() >= 3,
        "expected >=3 type nodes, got {}",
        class_nodes.len()
    );
    let method_nodes: Vec<_> = unit
        .nodes
        .iter()
        .filter(|n| {
            matches!(
                n.kind,
                cih_core::NodeKind::Method | cih_core::NodeKind::Function
            )
        })
        .collect();
    assert!(
        method_nodes.len() >= 2,
        "expected >=2 method nodes, got {}",
        method_nodes.len()
    );
}
