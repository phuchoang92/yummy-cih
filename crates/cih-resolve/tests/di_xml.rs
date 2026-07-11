use cih_core::{
    type_id, BindingKind, EdgeKind, NodeKind, ParsedFile, Range, SymbolDef, TypeBinding,
};
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
    // OSGi bundle layouts (SAP-OCB): anything under META-INF/spring/ is a
    // candidate; the content gate does the real filtering.
    assert!(is_di_xml_path(
        "custom-remittance/resources/META-INF/spring/bundle-context-rest.xml"
    ));
    assert!(is_di_xml_path(
        "platform/custom-remittance/resources/META-INF/spring/beans_rest_web_servlets.xml"
    ));
    assert!(!is_di_xml_path("pom.xml"));
    assert!(!is_di_xml_path("camel-routes.xml"));
    assert!(!is_di_xml_path("src/main/resources/other.xml"));
    assert!(!is_di_xml_path("META-INF/spring/readme.txt"));
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
fn spring_dm_reference_parses() {
    // Spring-DM (SAP-OCB style): namespaced <osgi:reference>/<osgi:service>,
    // often with no <bean> at all. Local-name matching must strip the prefix.
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans"
        xmlns:osgi="http://www.springframework.org/schema/osgi">
        <osgi:reference id="authenticationRef" interface="com.acme.auth.AuthenticationService"/>
        <osgi:service ref="remittanceServiceImpl" interface="com.acme.remit.RemittanceService"/>
    </beans>"#;
    let (beans, refs) = parse_di_document("META-INF/spring/bundle-context-rest-osgi.xml", xml);
    assert!(beans.is_empty());
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].interface, "com.acme.auth.AuthenticationService");
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

    assert!(out.edges.iter().any(|e| e.kind == EdgeKind::Calls
        && e.src == type_id(NodeKind::Class, consumer_fqcn)
        && e.dst == type_id(NodeKind::Class, "com.acme.OrderService")));
    assert!(out
        .nodes
        .iter()
        .any(|n| n.qualified_name.as_deref() == Some("com.acme.OrderService")));
}

#[test]
fn osgi_reference_in_meta_inf_spring_emits_calls_edge() {
    use cih_core::{RefKind, ReferenceSite};

    let dir = std::env::temp_dir().join(format!("cih-di-xml-osgi-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("META-INF/spring")).unwrap();
    // Spring-DM file with no <bean> element — previously skipped entirely.
    std::fs::write(
        dir.join("META-INF/spring/bundle-context-rest-osgi.xml"),
        r#"<beans xmlns="http://www.springframework.org/schema/beans"
            xmlns:osgi="http://www.springframework.org/schema/osgi">
            <osgi:reference id="apiRef" interface="com.acme.Api"/>
        </beans>"#,
    )
    .unwrap();

    fn def(kind: NodeKind, fqcn: &str, name: &str) -> SymbolDef {
        SymbolDef {
            id: type_id(kind, fqcn),
            kind,
            fqcn: fqcn.into(),
            name: name.into(),
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
        }
    }
    let parsed = vec![ParsedFile {
        file: "Api.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            def(NodeKind::Interface, "com.acme.Api", "Api"),
            def(NodeKind::Class, "com.acme.ApiImpl", "ApiImpl"),
        ],
        imports: vec![],
        reference_sites: vec![ReferenceSite {
            name: "Api".into(),
            receiver: None,
            kind: RefKind::Implements,
            arity: None,
            range: Range::default(),
            in_fqcn: "com.acme.ApiImpl".into(),
            in_callable: type_id(NodeKind::Class, "com.acme.ApiImpl"),
            arg_texts: vec![],
        }],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    }];

    let out = extract_di_xml(&dir, &parsed);
    let _ = std::fs::remove_dir_all(&dir);

    let edge = out
        .edges
        .iter()
        .find(|e| {
            e.kind == EdgeKind::Calls
                && e.src == type_id(NodeKind::Interface, "com.acme.Api")
                && e.dst == type_id(NodeKind::Class, "com.acme.ApiImpl")
        })
        .expect("interface -> implementor edge from <osgi:reference>");
    assert_eq!(edge.reason, "di-xml-blueprint-reference");
    assert!((edge.confidence - 0.7).abs() < f32::EPSILON);
}
