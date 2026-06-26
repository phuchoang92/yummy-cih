use cih_core::{type_id, BindingKind, EdgeKind, NodeKind, ParsedFile, Range, SymbolDef, TypeBinding};
use cih_resolve::di_xml::{
    extract_di_xml, extract_xml_attr, is_di_xml_path, parse_di_document, simple_name,
};

#[test]
fn detects_di_xml_paths() {
    assert!(is_di_xml_path("src/main/resources/applicationContext.xml"));
    assert!(is_di_xml_path("applicationContext-web.xml"));
    assert!(is_di_xml_path("beans.xml"));
    assert!(is_di_xml_path("conf/blueprint.xml"));
    assert!(is_di_xml_path("OSGI-INF/blueprint/wiring.xml"));
    assert!(!is_di_xml_path("pom.xml"));
    assert!(!is_di_xml_path("camel-routes.xml"));
}

#[test]
fn simple_name_strips_qualifiers() {
    assert_eq!(simple_name("com.acme.OrderService"), "OrderService");
    assert_eq!(simple_name("List<Foo>"), "List");
    assert_eq!(simple_name("Foo[]"), "Foo");
    assert_eq!(simple_name("Bar"), "Bar");
}

#[test]
fn parses_beans_and_references() {
    let xml = r#"<blueprint xmlns="http://www.osgi.org/xmlns/blueprint">
        <bean id="orderService" class="com.acme.OrderServiceImpl"/>
        <reference id="repo" interface="com.acme.OrderRepository"/>
        <service ref="orderService" interface="com.acme.OrderService"/>
    </blueprint>"#;
    let (beans, refs) = parse_di_document("blueprint.xml", xml);
    assert_eq!(beans.len(), 1);
    assert_eq!(beans[0].fqcn, "com.acme.OrderServiceImpl");
    assert_eq!(beans[0].id.as_deref(), Some("orderService"));
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].interface, "com.acme.OrderRepository");
}

#[test]
fn extract_xml_attr_respects_boundaries() {
    assert_eq!(
        extract_xml_attr("<bean class=\"com.acme.Foo\"/>", "class").as_deref(),
        Some("com.acme.Foo")
    );
    assert_eq!(extract_xml_attr("<bean myclass=\"x\"/>", "class"), None);
}

#[test]
fn field_injection_emits_calls_edge() {
    let dir = std::env::temp_dir().join(format!("cih-di-xml-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("applicationContext.xml"),
        r#"<beans xmlns="http://www.springframework.org/schema/beans">
            <bean id="orderService" class="com.acme.OrderService"/>
        </beans>"#,
    )
    .unwrap();

    let consumer_fqcn = "com.acme.OrderController";
    let parsed = vec![ParsedFile {
        file: "OrderController.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![SymbolDef {
            id: type_id(NodeKind::Class, consumer_fqcn),
            kind: NodeKind::Class,
            fqcn: consumer_fqcn.into(),
            name: "OrderController".into(),
            owner: None,
            range: Range::default(),
            modifiers: vec![],
            param_types: vec![],
            return_type: None,
            declared_type: None,
            framework_role: None,
            body_fingerprint: None,
            complexity: None,
            lang_meta: None,
        }],
        imports: vec![],
        reference_sites: vec![],
        type_bindings: vec![TypeBinding {
            name: "orderService".into(),
            raw_type: "OrderService".into(),
            kind: BindingKind::Field,
            in_fqcn: consumer_fqcn.into(),
            range: Range::default(),
        }],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    }];

    let out = extract_di_xml(&dir, &parsed);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(out
        .edges
        .iter()
        .any(|e| e.kind == EdgeKind::Calls
            && e.src == type_id(NodeKind::Class, consumer_fqcn)
            && e.dst == type_id(NodeKind::Class, "com.acme.OrderService")));
    assert!(out
        .nodes
        .iter()
        .any(|n| n.qualified_name.as_deref() == Some("com.acme.OrderService")));
}
