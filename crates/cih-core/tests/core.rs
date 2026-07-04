use cih_core::{
    community_id, constructor_id, external_endpoint_id, field_id, file_id, folder_id,
    kafka_topic_id, method_id, process_id, type_id,
};
use cih_core::{
    ArchitectureHint, BindingKind, BuildSystem, ContractKind, ContractSite, JarInfo, ModuleInfo,
    NodeKind, ParsedFile, RawImport, Range, ReferenceSite, RefKind, RepoMap, SymbolDef,
    TypeBinding,
};

#[test]
fn id_helpers_use_locked_scheme() {
    assert_eq!(
        file_id("src/main/java/App.java").as_str(),
        "File:src/main/java/App.java"
    );
    assert_eq!(folder_id("src/main/java").as_str(), "Folder:src/main/java");
    assert_eq!(
        type_id(NodeKind::Class, "com.acme.Outer.Inner").as_str(),
        "Class:com.acme.Outer.Inner"
    );
    assert_eq!(
        type_id(NodeKind::Interface, "com.acme.Service").as_str(),
        "Interface:com.acme.Service"
    );
    assert_eq!(
        method_id("com.acme.Outer.Inner", "save", 2).as_str(),
        "Method:com.acme.Outer.Inner#save/2"
    );
    assert_eq!(
        constructor_id("com.acme.Outer.Inner", 1).as_str(),
        "Constructor:com.acme.Outer.Inner#<init>/1"
    );
    assert_eq!(
        field_id("com.acme.Outer.Inner", "name").as_str(),
        "Field:com.acme.Outer.Inner#name"
    );
    assert_eq!(community_id(3).as_str(), "Community:3");
    assert_eq!(
        process_id("handle-login", "a3f9c1").as_str(),
        "Process:handle-login-a3f9c1"
    );
    assert_eq!(kafka_topic_id("orders").as_str(), "KafkaTopic:orders");
    assert_eq!(
        external_endpoint_id("get", "/api/orders/{id}").as_str(),
        "ExternalEndpoint:GET:/api/orders/{id}"
    );
}

#[test]
fn node_kind_labels_round_trip() {
    for kind in [
        NodeKind::File,
        NodeKind::Folder,
        NodeKind::Class,
        NodeKind::Interface,
        NodeKind::Enum,
        NodeKind::Record,
        NodeKind::Annotation,
        NodeKind::Method,
        NodeKind::Function,
        NodeKind::Constructor,
        NodeKind::Field,
        NodeKind::Route,
        NodeKind::Community,
        NodeKind::Process,
        NodeKind::KafkaTopic,
        NodeKind::ExternalEndpoint,
        NodeKind::DbQuery,
        NodeKind::DbTable,
        NodeKind::IntegrationRoute,
        NodeKind::MessageDestination,
        NodeKind::Other,
    ] {
        assert_eq!(NodeKind::from_label(kind.label()), kind);
    }
    assert_eq!(NodeKind::from_label("Unknown"), NodeKind::Other);
}

#[test]
fn db_id_helpers_use_locked_scheme() {
    use cih_core::{db_query_const_id, db_query_inline_id, db_table_id};
    assert_eq!(
        db_query_const_id("com.bank.OverdraftAdapterImpl", "QUERY_FOO").as_str(),
        "DbQuery:com.bank.OverdraftAdapterImpl#QUERY_FOO"
    );
    assert_eq!(
        db_query_inline_id("src/main/java/Adapter.java", 42, 8).as_str(),
        "DbQuery:src/main/java/Adapter.java:42:8"
    );
    assert_eq!(
        db_table_id("custom_overdraft_type").as_str(),
        "DbTable:CUSTOM_OVERDRAFT_TYPE"
    );
    assert_eq!(
        db_table_id("CUSTOM_OVERDRAFT").as_str(),
        "DbTable:CUSTOM_OVERDRAFT"
    );
}

#[test]
fn repo_map_round_trips_json() {
    let mut per_lang = std::collections::BTreeMap::new();
    per_lang.insert("java".into(), 3);

    let repo_map = RepoMap {
        root: "/repo".into(),
        build_system: BuildSystem::Maven,
        total_source_loc: 120,
        modules: vec![ModuleInfo {
            name: "app".into(),
            rel_path: ".".into(),
            build_file: Some("pom.xml".into()),
            source_files: 3,
            source_loc: 120,
            packages: vec!["com.acme".into()],
            depends_on: vec!["core".into()],
            frameworks: vec!["spring".into()],
            per_language: per_lang.clone(),
        }],
        jars: vec![JarInfo {
            path: "lib/example.jar".into(),
            group_id: Some("com.acme".into()),
            artifact: Some("example".into()),
            is_own: true,
            classes: 12,
        }],
        decompiled_dirs: vec![".workspace-dependencies".into()],
        architecture_hint: ArchitectureHint::Unknown,
        total_source_files: 3,
        per_language: per_lang,
    };

    let encoded = serde_json::to_string(&repo_map).unwrap();
    let decoded: RepoMap = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, repo_map);
}

#[test]
fn parsed_file_round_trips_json() {
    let parsed = ParsedFile {
        file: "src/main/java/com/acme/UserService.java".into(),
        language: "java".into(),
        package: Some("com.acme".into()),
        defs: vec![SymbolDef {
            id: method_id("com.acme.UserService", "save", 1),
            kind: NodeKind::Method,
            fqcn: "com.acme.UserService".into(),
            name: "save".into(),
            owner: Some(type_id(NodeKind::Class, "com.acme.UserService")),
            range: Range {
                start_line: 10,
                start_col: 4,
                end_line: 12,
                end_col: 5,
            },
            modifiers: vec!["public".into()],
            param_types: vec!["Long".into()],
            return_type: Some("User".into()),
            declared_type: None,
            framework_role: None,
            body_fingerprint: None,
            complexity: None,
            lang_meta: None,
        }],
        imports: vec![RawImport {
            raw: "java.util.List".into(),
            is_static: false,
            is_wildcard: false,
            range: Range {
                start_line: 3,
                start_col: 0,
                end_line: 3,
                end_col: 22,
            },
        }],
        reference_sites: vec![ReferenceSite {
            name: "findById".into(),
            receiver: Some("repository".into()),
            kind: RefKind::Call,
            arity: Some(1),
            range: Range {
                start_line: 11,
                start_col: 16,
                end_line: 11,
                end_col: 24,
            },
            in_fqcn: "com.acme.UserService#save/1".into(),
            in_callable: method_id("com.acme.UserService", "save", 1),
            arg_texts: vec![],
        }],
        type_bindings: vec![TypeBinding {
            name: "repository".into(),
            raw_type: "UserRepository".into(),
            kind: BindingKind::Field,
            in_fqcn: "com.acme.UserService".into(),
            range: Range {
                start_line: 6,
                start_col: 4,
                end_line: 6,
                end_col: 40,
            },
        }],
        contract_sites: vec![ContractSite {
            kind: ContractKind::EventPublish,
            url_template: None,
            topic: Some("user-saved".into()),
            http_method: None,
            in_callable: method_id("com.acme.UserService", "save", 1),
            range: Range {
                start_line: 12,
                start_col: 8,
                end_line: 12,
                end_col: 40,
            },
        }],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };

    let encoded = serde_json::to_string(&parsed).unwrap();
    let decoded: ParsedFile = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, parsed);
}
