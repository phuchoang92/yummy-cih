use cih_core::{
    constructor_id, external_endpoint_id, field_id, file_id, function_id, kafka_topic_id,
    method_id, type_id, BindingKind, ContractKind, ContractSite, EdgeKind, MessagingFramework,
    NodeId, NodeKind, ParsedFile, Range, RawImport, RefKind, ReferenceSite, StringConstant,
    SymbolDef, TypeBinding, UrlPart,
};
use cih_resolve::{
    build_java_constant_resolver, default_registry, resolve_edges, resolve_with_registry,
    ResolveOptions,
};

fn simple_of(fqcn: &str) -> String {
    fqcn.rsplit('.').next().unwrap_or(fqcn).to_string()
}

fn type_def(kind: NodeKind, fqcn: &str) -> SymbolDef {
    SymbolDef {
        id: type_id(kind, fqcn),
        kind,
        fqcn: fqcn.into(),
        name: simple_of(fqcn),
        owner: None,
        range: Range::default(),
        modifiers: Vec::new(),
        param_types: Vec::new(),
        return_type: None,
        declared_type: None,
        framework_role: None,
        body_fingerprint: None,
        complexity: None,
        lang_meta: None,
    }
}

fn method_def(owner: &str, name: &str, params: &[&str], ret: Option<&str>) -> SymbolDef {
    SymbolDef {
        id: method_id(owner, name, params.len() as u16),
        kind: NodeKind::Method,
        fqcn: owner.into(),
        name: name.into(),
        owner: Some(type_id(NodeKind::Class, owner)),
        range: Range::default(),
        modifiers: Vec::new(),
        param_types: params.iter().map(|s| s.to_string()).collect(),
        return_type: ret.map(str::to_string),
        declared_type: None,
        framework_role: None,
        body_fingerprint: None,
        complexity: None,
        lang_meta: None,
    }
}

fn field_def(owner: &str, name: &str, ty: &str) -> SymbolDef {
    SymbolDef {
        id: field_id(owner, name),
        kind: NodeKind::Field,
        fqcn: owner.into(),
        name: name.into(),
        owner: Some(type_id(NodeKind::Class, owner)),
        range: Range::default(),
        modifiers: Vec::new(),
        param_types: Vec::new(),
        return_type: None,
        declared_type: Some(ty.into()),
        framework_role: None,
        body_fingerprint: None,
        complexity: None,
        lang_meta: None,
    }
}

fn ctor_def(owner: &str, params: &[&str]) -> SymbolDef {
    SymbolDef {
        id: constructor_id(owner, params.len() as u16),
        kind: NodeKind::Constructor,
        fqcn: owner.into(),
        name: "<init>".into(),
        owner: Some(type_id(NodeKind::Class, owner)),
        range: Range::default(),
        modifiers: Vec::new(),
        param_types: params.iter().map(|s| s.to_string()).collect(),
        return_type: None,
        declared_type: None,
        framework_role: None,
        body_fingerprint: None,
        complexity: None,
        lang_meta: None,
    }
}

fn binding(name: &str, raw: &str, kind: BindingKind, in_fqcn: &str, line: u32) -> TypeBinding {
    TypeBinding {
        name: name.into(),
        raw_type: raw.into(),
        kind,
        in_fqcn: in_fqcn.into(),
        range: Range {
            start_line: line,
            ..Range::default()
        },
    }
}

fn import(raw: &str) -> RawImport {
    RawImport {
        raw: raw.into(),
        is_static: false,
        is_wildcard: raw.ends_with(".*"),
        range: Range::default(),
    }
}

fn heritage(class_fqcn: &str, super_name: &str, kind: RefKind) -> ReferenceSite {
    ReferenceSite {
        name: super_name.into(),
        receiver: None,
        kind,
        arity: None,
        range: Range::default(),
        in_fqcn: class_fqcn.into(),
        in_callable: type_id(NodeKind::Class, class_fqcn),
        arg_texts: vec![],
    }
}

fn ref_site(
    in_fqcn: &str,
    in_callable: NodeId,
    kind: RefKind,
    receiver: Option<&str>,
    name: &str,
    arity: Option<u16>,
) -> ReferenceSite {
    ReferenceSite {
        name: name.into(),
        receiver: receiver.map(str::to_string),
        kind,
        arity,
        range: Range::default(),
        in_fqcn: in_fqcn.into(),
        in_callable,
        arg_texts: vec![],
    }
}

fn workspace() -> Vec<ParsedFile> {
    let repo = ParsedFile {
        file: "com/acme/Repo.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            type_def(NodeKind::Interface, "com.acme.Repo"),
            method_def("com.acme.Repo", "findAll", &[], Some("List")),
        ],
        imports: vec![import("java.util.List")],
        reference_sites: vec![],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let service = ParsedFile {
        file: "com/acme/OwnerService.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            type_def(NodeKind::Class, "com.acme.OwnerService"),
            method_def("com.acme.OwnerService", "save", &["Owner"], None),
            method_def("com.acme.OwnerService", "save", &["Owner", "boolean"], None),
        ],
        imports: vec![],
        reference_sites: vec![heritage(
            "com.acme.OwnerService",
            "Repo",
            RefKind::Implements,
        )],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let controller = ParsedFile {
        file: "com/acme/OwnerController.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            type_def(NodeKind::Class, "com.acme.OwnerController"),
            type_def(NodeKind::Class, "com.acme.Owner"),
            ctor_def("com.acme.Owner", &[]),
            field_def("com.acme.OwnerController", "service", "OwnerService"),
            method_def(
                "com.acme.OwnerController",
                "handle",
                &["OwnerService"],
                None,
            ),
        ],
        imports: vec![import("java.util.List"), import("com.other.Thing")],
        reference_sites: vec![],
        type_bindings: vec![binding(
            "svc",
            "OwnerService",
            BindingKind::Param,
            "com.acme.OwnerController#handle/1",
            5,
        )],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let thing = ParsedFile {
        file: "com/other/Thing.java".into(),
        language: String::new(),
        package: Some("com.other".into()),
        defs: vec![type_def(NodeKind::Class, "com.other.Thing")],
        imports: vec![],
        reference_sites: vec![],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    vec![repo, service, controller, thing]
}

#[test]
fn contract_sites_emit_nodes_and_edges() {
    let caller = method_id("com.acme.Client", "call", 0);
    let listener = method_id("com.acme.Client", "listen", 1);
    let file = ParsedFile {
        file: "com/acme/Client.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![],
        imports: vec![],
        reference_sites: vec![],
        type_bindings: vec![],
        contract_sites: vec![
            ContractSite {
                kind: ContractKind::HttpCall,
                url_template: Some("/api/orders/{id}".into()),
                topic: None,
                http_method: Some("get".into()),
                messaging_framework: None,
                url_parts: None,
                in_callable: caller.clone(),
                range: Range::default(),
            },
            ContractSite {
                kind: ContractKind::EventPublish,
                url_template: None,
                topic: Some("orders.created".into()),
                http_method: None,
                messaging_framework: Some(MessagingFramework::Kafka),
                url_parts: None,
                in_callable: caller.clone(),
                range: Range::default(),
            },
            ContractSite {
                kind: ContractKind::EventListen,
                url_template: None,
                topic: Some("orders.created".into()),
                http_method: None,
                messaging_framework: Some(MessagingFramework::Spring),
                url_parts: None,
                in_callable: listener.clone(),
                range: Range::default(),
            },
        ],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };

    let out = resolve_edges(&[file]);
    let endpoint = external_endpoint_id("GET", "/api/orders/{id}");
    let topic = kafka_topic_id("orders.created");
    assert!(out
        .nodes
        .iter()
        .any(|node| node.id == endpoint && node.kind == NodeKind::ExternalEndpoint));
    assert!(out
        .nodes
        .iter()
        .any(|node| node.id == topic && node.kind == NodeKind::KafkaTopic));
    assert!(out.edges.iter().any(|edge| {
        edge.kind == EdgeKind::ExternalCall && edge.src == caller && edge.dst == endpoint
    }));
    let publish = out
        .edges
        .iter()
        .find(|edge| {
            edge.kind == EdgeKind::PublishesEvent && edge.src == caller && edge.dst == topic
        })
        .expect("PublishesEvent edge expected");
    assert_eq!(
        publish
            .props
            .as_ref()
            .and_then(|p| p.get("messaging_framework"))
            .and_then(|v| v.as_str()),
        Some("kafka"),
        "Kafka publish should carry messaging_framework=kafka"
    );
    let listen = out
        .edges
        .iter()
        .find(|edge| edge.kind == EdgeKind::ListensTo && edge.src == listener && edge.dst == topic)
        .expect("ListensTo edge expected");
    assert_eq!(
        listen
            .props
            .as_ref()
            .and_then(|p| p.get("messaging_framework"))
            .and_then(|v| v.as_str()),
        Some("spring"),
        "Spring listener should carry messaging_framework=spring"
    );
}

fn mro_workspace() -> Vec<ParsedFile> {
    let animal = ParsedFile {
        file: "com/acme/Animal.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            type_def(NodeKind::Interface, "com.acme.Animal"),
            method_def("com.acme.Animal", "speak", &[], None),
        ],
        imports: vec![],
        reference_sites: vec![],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let mammal = ParsedFile {
        file: "com/acme/Mammal.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            type_def(NodeKind::Class, "com.acme.Mammal"),
            method_def("com.acme.Mammal", "breathe", &[], None),
        ],
        imports: vec![],
        reference_sites: vec![heritage("com.acme.Mammal", "Animal", RefKind::Implements)],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let dog = ParsedFile {
        file: "com/acme/Dog.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            type_def(NodeKind::Class, "com.acme.Dog"),
            method_def("com.acme.Dog", "speak", &[], None),
            method_def("com.acme.Dog", "breathe", &[], None),
        ],
        imports: vec![],
        reference_sites: vec![
            heritage("com.acme.Dog", "Mammal", RefKind::Extends),
            heritage("com.acme.Dog", "Animal", RefKind::Implements),
        ],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    vec![animal, mammal, dog]
}

#[test]
fn phase_4_2_receiver_bound_call_emits_calls_edge() {
    let mut files = workspace();
    let scope = "com.acme.OwnerController#handle/1";
    files[2].reference_sites.push(ref_site(
        scope,
        method_id("com.acme.OwnerController", "handle", 1),
        RefKind::Call,
        Some("service"),
        "save",
        Some(1),
    ));

    let out = resolve_edges(&files);
    assert!(
        out.edges.iter().any(|edge| {
            edge.kind == EdgeKind::Calls
                && edge.src == method_id("com.acme.OwnerController", "handle", 1)
                && edge.dst == method_id("com.acme.OwnerService", "save", 1)
        }),
        "service.save(owner) should resolve to OwnerService#save/1"
    );
    assert_eq!(out.skipped, 0);
}

#[test]
fn phase_4_2_free_calls_imports_heritage_fields_and_ctors() {
    let mut files = workspace();
    files[1].reference_sites.push(ref_site(
        "com.acme.OwnerService#save/1",
        method_id("com.acme.OwnerService", "save", 1),
        RefKind::Call,
        None,
        "findAll",
        Some(0),
    ));
    files[2].reference_sites.push(ref_site(
        "com.acme.OwnerController#handle/1",
        method_id("com.acme.OwnerController", "handle", 1),
        RefKind::FieldRead,
        Some("this"),
        "service",
        None,
    ));
    files[2].reference_sites.push(ref_site(
        "com.acme.OwnerController#handle/1",
        method_id("com.acme.OwnerController", "handle", 1),
        RefKind::Ctor,
        None,
        "Owner",
        Some(0),
    ));

    let out = resolve_edges(&files);
    assert!(out.edges.iter().any(|edge| {
        edge.kind == EdgeKind::Calls
            && edge.src == method_id("com.acme.OwnerService", "save", 1)
            && edge.dst == method_id("com.acme.Repo", "findAll", 0)
    }));
    assert!(out.edges.iter().any(|edge| {
        edge.kind == EdgeKind::Accesses
            && edge.src == method_id("com.acme.OwnerController", "handle", 1)
            && edge.dst == field_id("com.acme.OwnerController", "service")
    }));
    assert!(out.edges.iter().any(|edge| {
        edge.kind == EdgeKind::Calls
            && edge.src == method_id("com.acme.OwnerController", "handle", 1)
            && edge.dst == constructor_id("com.acme.Owner", 0)
    }));
    assert!(out.edges.iter().any(|edge| {
        edge.kind == EdgeKind::Imports
            && edge.src == file_id("com/acme/OwnerController.java")
            && edge.dst == type_id(NodeKind::Class, "com.other.Thing")
    }));
    assert!(out.edges.iter().any(|edge| {
        edge.kind == EdgeKind::Implements
            && edge.src == type_id(NodeKind::Class, "com.acme.OwnerService")
            && edge.dst == type_id(NodeKind::Interface, "com.acme.Repo")
    }));
    assert_eq!(out.skipped, 0);
}

#[test]
fn phase_4_2_unresolved_external_receiver_is_reported() {
    let mut files = workspace();
    let scope = "com.acme.OwnerController#handle/1";
    files[2].type_bindings.push(binding(
        "client",
        "com.external.Client",
        BindingKind::Local,
        scope,
        7,
    ));
    files[2].reference_sites.push(ref_site(
        scope,
        method_id("com.acme.OwnerController", "handle", 1),
        RefKind::Call,
        Some("client"),
        "fetch",
        Some(0),
    ));

    let out = resolve_edges(&files);
    assert_eq!(out.skipped, 1);
    assert_eq!(out.unresolved_external_fqcns, vec!["com.external.Client"]);
}

#[test]
fn phase_4_3_mro_method_implements_interface() {
    let files = mro_workspace();
    let out = resolve_edges(&files);
    assert!(
        out.edges.iter().any(|e| {
            e.kind == EdgeKind::MethodImplements
                && e.src == method_id("com.acme.Dog", "speak", 0)
                && e.dst == method_id("com.acme.Animal", "speak", 0)
        }),
        "Dog.speak should METHOD_IMPLEMENTS Animal.speak"
    );
}

#[test]
fn phase_4_3_mro_method_overrides_superclass() {
    let files = mro_workspace();
    let out = resolve_edges(&files);
    assert!(
        out.edges.iter().any(|e| {
            e.kind == EdgeKind::MethodOverrides
                && e.src == method_id("com.acme.Dog", "breathe", 0)
                && e.dst == method_id("com.acme.Mammal", "breathe", 0)
        }),
        "Dog.breathe should METHOD_OVERRIDES Mammal.breathe"
    );
}

#[test]
fn phase_4_3_mro_both_overrides_and_implements() {
    let mut files = mro_workspace();
    files[1]
        .defs
        .push(method_def("com.acme.Mammal", "speak", &[], None));
    let out = resolve_edges(&files);
    assert!(
        out.edges.iter().any(|e| {
            e.kind == EdgeKind::MethodOverrides
                && e.src == method_id("com.acme.Dog", "speak", 0)
                && e.dst == method_id("com.acme.Mammal", "speak", 0)
        }),
        "Dog.speak should METHOD_OVERRIDES Mammal.speak"
    );
    assert!(
        out.edges.iter().any(|e| {
            e.kind == EdgeKind::MethodImplements
                && e.src == method_id("com.acme.Dog", "speak", 0)
                && e.dst == method_id("com.acme.Animal", "speak", 0)
        }),
        "Dog.speak should also METHOD_IMPLEMENTS Animal.speak"
    );
}

#[test]
fn phase_4_3_c3_order_superclass_before_interface() {
    let base = ParsedFile {
        file: "com/acme/Base.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            type_def(NodeKind::Class, "com.acme.Base"),
            method_def("com.acme.Base", "act", &[], None),
        ],
        imports: vec![],
        reference_sites: vec![],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let marker = ParsedFile {
        file: "com/acme/Marker.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            type_def(NodeKind::Interface, "com.acme.Marker"),
            method_def("com.acme.Marker", "act", &[], None),
        ],
        imports: vec![],
        reference_sites: vec![],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let child = ParsedFile {
        file: "com/acme/Child.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            type_def(NodeKind::Class, "com.acme.Child"),
            method_def("com.acme.Child", "act", &[], None),
        ],
        imports: vec![],
        reference_sites: vec![
            heritage("com.acme.Child", "Base", RefKind::Extends),
            heritage("com.acme.Child", "Marker", RefKind::Implements),
        ],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let out = resolve_edges(&[base, marker, child]);
    assert!(
        out.edges.iter().any(|e| {
            e.kind == EdgeKind::MethodOverrides
                && e.src == method_id("com.acme.Child", "act", 0)
                && e.dst == method_id("com.acme.Base", "act", 0)
        }),
        "Child.act should METHOD_OVERRIDES Base.act"
    );
    assert!(
        out.edges.iter().any(|e| {
            e.kind == EdgeKind::MethodImplements
                && e.src == method_id("com.acme.Child", "act", 0)
                && e.dst == method_id("com.acme.Marker", "act", 0)
        }),
        "Child.act should METHOD_IMPLEMENTS Marker.act"
    );
    assert!(
        !out.edges.iter().any(|e| {
            e.kind == EdgeKind::MethodOverrides && e.dst == method_id("com.acme.Marker", "act", 0)
        }),
        "should not emit METHOD_OVERRIDES to an interface"
    );
}

fn make_di_scenario(impl_stereotype: Option<&str>) -> Vec<ParsedFile> {
    let iface = ParsedFile {
        file: "com/acme/UserService.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            SymbolDef {
                id: type_id(NodeKind::Interface, "com.acme.UserService"),
                kind: NodeKind::Interface,
                fqcn: "com.acme.UserService".into(),
                name: "UserService".into(),
                owner: None,
                range: Range::default(),
                modifiers: Vec::new(),
                param_types: Vec::new(),
                return_type: None,
                declared_type: None,
                framework_role: None,
                body_fingerprint: None,
                complexity: None,
                lang_meta: None,
            },
            method_def("com.acme.UserService", "save", &["User"], None),
        ],
        imports: vec![],
        reference_sites: vec![],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let impl_def = SymbolDef {
        id: type_id(NodeKind::Class, "com.acme.UserServiceImpl"),
        kind: NodeKind::Class,
        fqcn: "com.acme.UserServiceImpl".into(),
        name: "UserServiceImpl".into(),
        owner: None,
        range: Range::default(),
        modifiers: Vec::new(),
        param_types: Vec::new(),
        return_type: None,
        declared_type: None,
        framework_role: impl_stereotype.map(str::to_string),
        body_fingerprint: None,
        complexity: None,
        lang_meta: None,
    };
    let impl_file = ParsedFile {
        file: "com/acme/UserServiceImpl.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            impl_def,
            method_def("com.acme.UserServiceImpl", "save", &["User"], None),
        ],
        imports: vec![],
        reference_sites: vec![heritage(
            "com.acme.UserServiceImpl",
            "UserService",
            RefKind::Implements,
        )],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let caller = ParsedFile {
        file: "com/acme/OrderController.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            SymbolDef {
                id: type_id(NodeKind::Class, "com.acme.OrderController"),
                kind: NodeKind::Class,
                fqcn: "com.acme.OrderController".into(),
                name: "OrderController".into(),
                owner: None,
                range: Range::default(),
                modifiers: Vec::new(),
                param_types: Vec::new(),
                return_type: None,
                declared_type: None,
                framework_role: Some("controller".into()),
                body_fingerprint: None,
                complexity: None,
                lang_meta: None,
            },
            method_def("com.acme.OrderController", "placeOrder", &["Order"], None),
            field_def("com.acme.OrderController", "userService", "UserService"),
        ],
        imports: vec![],
        reference_sites: vec![ReferenceSite {
            name: "save".into(),
            receiver: Some("userService".into()),
            kind: RefKind::Call,
            arity: Some(1),
            range: Range::default(),
            in_fqcn: "com.acme.OrderController#placeOrder/1".into(),
            in_callable: method_id("com.acme.OrderController", "placeOrder", 1),
            arg_texts: vec![],
        }],
        type_bindings: vec![TypeBinding {
            name: "userService".into(),
            raw_type: "UserService".into(),
            kind: BindingKind::Field,
            in_fqcn: "com.acme.OrderController".into(),
            range: Range::default(),
        }],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    vec![iface, impl_file, caller]
}

#[test]
fn di_resolves_interface_call_to_service_impl() {
    let files = make_di_scenario(Some("service"));
    let out = resolve_edges(&files);
    let calls: Vec<_> = out
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .collect();
    assert!(
        calls
            .iter()
            .any(|e| e.dst == method_id("com.acme.UserServiceImpl", "save", 1)),
        "should resolve to impl method"
    );
    assert!(
        !calls
            .iter()
            .any(|e| e.dst == method_id("com.acme.UserService", "save", 1)),
        "should NOT call the interface method when impl is found"
    );
    let di_edge = calls
        .iter()
        .find(|e| e.dst == method_id("com.acme.UserServiceImpl", "save", 1))
        .unwrap();
    assert_eq!(di_edge.reason, "di-resolved");
}

/// A single concrete implementor gets the DI redirect even without a
/// `@Service` stereotype (annotation-driven wiring may leave metadata empty);
/// the interface fallback is reserved for the ambiguous multi-impl case,
/// covered by `di_falls_back_when_multiple_service_impls`.
#[test]
fn di_redirects_to_single_impl_even_without_stereotype() {
    let files = make_di_scenario(None);
    let out = resolve_edges(&files);
    let calls: Vec<_> = out
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .collect();
    let di_edge = calls
        .iter()
        .find(|e| e.dst == method_id("com.acme.UserServiceImpl", "save", 1))
        .expect("single un-stereotyped impl should still receive the DI redirect");
    assert_eq!(di_edge.reason, "di-resolved");
}

#[test]
fn di_falls_back_when_multiple_service_impls() {
    let iface = ParsedFile {
        file: "com/acme/UserService.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            SymbolDef {
                id: type_id(NodeKind::Interface, "com.acme.UserService"),
                kind: NodeKind::Interface,
                fqcn: "com.acme.UserService".into(),
                name: "UserService".into(),
                owner: None,
                range: Range::default(),
                modifiers: Vec::new(),
                param_types: Vec::new(),
                return_type: None,
                declared_type: None,
                framework_role: None,
                body_fingerprint: None,
                complexity: None,
                lang_meta: None,
            },
            method_def("com.acme.UserService", "save", &["User"], None),
        ],
        imports: vec![],
        reference_sites: vec![],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let make_impl = |name: &str| -> ParsedFile {
        let fqcn = format!("com.acme.{name}");
        ParsedFile {
            file: format!("com/acme/{name}.java"),
            language: String::new(),
            package: Some("com.acme".into()),
            defs: vec![
                SymbolDef {
                    id: type_id(NodeKind::Class, &fqcn),
                    kind: NodeKind::Class,
                    fqcn: fqcn.clone(),
                    name: name.to_string(),
                    owner: None,
                    range: Range::default(),
                    modifiers: Vec::new(),
                    param_types: Vec::new(),
                    return_type: None,
                    declared_type: None,
                    framework_role: Some("service".into()),
                    body_fingerprint: None,
                    complexity: None,
                    lang_meta: None,
                },
                method_def(&fqcn, "save", &["User"], None),
            ],
            imports: vec![],
            reference_sites: vec![heritage(&fqcn, "UserService", RefKind::Implements)],
            type_bindings: vec![],
            contract_sites: vec![],
            sql_constants: vec![],
            sql_execution_sites: vec![],
            string_constants: vec![],
        }
    };
    let caller = ParsedFile {
        file: "com/acme/OrderController.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            SymbolDef {
                id: type_id(NodeKind::Class, "com.acme.OrderController"),
                kind: NodeKind::Class,
                fqcn: "com.acme.OrderController".into(),
                name: "OrderController".into(),
                owner: None,
                range: Range::default(),
                modifiers: Vec::new(),
                param_types: Vec::new(),
                return_type: None,
                declared_type: None,
                framework_role: Some("controller".into()),
                body_fingerprint: None,
                complexity: None,
                lang_meta: None,
            },
            method_def("com.acme.OrderController", "placeOrder", &["Order"], None),
            field_def("com.acme.OrderController", "userService", "UserService"),
        ],
        imports: vec![],
        reference_sites: vec![ReferenceSite {
            name: "save".into(),
            receiver: Some("userService".into()),
            kind: RefKind::Call,
            arity: Some(1),
            range: Range::default(),
            in_fqcn: "com.acme.OrderController#placeOrder/1".into(),
            in_callable: method_id("com.acme.OrderController", "placeOrder", 1),
            arg_texts: vec![],
        }],
        type_bindings: vec![TypeBinding {
            name: "userService".into(),
            raw_type: "UserService".into(),
            kind: BindingKind::Field,
            in_fqcn: "com.acme.OrderController".into(),
            range: Range::default(),
        }],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let out = resolve_edges(&[
        iface,
        make_impl("UserServiceImplA"),
        make_impl("UserServiceImplB"),
        caller,
    ]);
    let calls: Vec<_> = out
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .collect();
    assert!(
        calls
            .iter()
            .any(|e| e.dst == method_id("com.acme.UserService", "save", 1)),
        "ambiguous DI should fall back to interface method"
    );
    assert!(
        !calls.iter().any(|e| e.reason == "di-resolved"),
        "should not emit di-resolved edge when ambiguous"
    );
}

#[test]
fn di_not_applied_to_concrete_class_receiver() {
    let files = {
        let concrete = ParsedFile {
            file: "com/acme/UserServiceImpl.java".into(),
            language: String::new(),
            package: Some("com.acme".into()),
            defs: vec![
                SymbolDef {
                    id: type_id(NodeKind::Class, "com.acme.UserServiceImpl"),
                    kind: NodeKind::Class,
                    fqcn: "com.acme.UserServiceImpl".into(),
                    name: "UserServiceImpl".into(),
                    owner: None,
                    range: Range::default(),
                    modifiers: Vec::new(),
                    param_types: Vec::new(),
                    return_type: None,
                    declared_type: None,
                    framework_role: Some("service".into()),
                    body_fingerprint: None,
                    complexity: None,
                    lang_meta: None,
                },
                method_def("com.acme.UserServiceImpl", "save", &["User"], None),
            ],
            imports: vec![],
            reference_sites: vec![],
            type_bindings: vec![],
            contract_sites: vec![],
            sql_constants: vec![],
            sql_execution_sites: vec![],
            string_constants: vec![],
        };
        let caller = ParsedFile {
            file: "com/acme/OrderController.java".into(),
            language: String::new(),
            package: Some("com.acme".into()),
            defs: vec![
                SymbolDef {
                    id: type_id(NodeKind::Class, "com.acme.OrderController"),
                    kind: NodeKind::Class,
                    fqcn: "com.acme.OrderController".into(),
                    name: "OrderController".into(),
                    owner: None,
                    range: Range::default(),
                    modifiers: Vec::new(),
                    param_types: Vec::new(),
                    return_type: None,
                    declared_type: None,
                    framework_role: Some("controller".into()),
                    body_fingerprint: None,
                    complexity: None,
                    lang_meta: None,
                },
                method_def("com.acme.OrderController", "placeOrder", &["Order"], None),
                field_def(
                    "com.acme.OrderController",
                    "userServiceImpl",
                    "UserServiceImpl",
                ),
            ],
            imports: vec![],
            reference_sites: vec![ReferenceSite {
                name: "save".into(),
                receiver: Some("userServiceImpl".into()),
                kind: RefKind::Call,
                arity: Some(1),
                range: Range::default(),
                in_fqcn: "com.acme.OrderController#placeOrder/1".into(),
                in_callable: method_id("com.acme.OrderController", "placeOrder", 1),
                arg_texts: vec![],
            }],
            type_bindings: vec![TypeBinding {
                name: "userServiceImpl".into(),
                raw_type: "UserServiceImpl".into(),
                kind: BindingKind::Field,
                in_fqcn: "com.acme.OrderController".into(),
                range: Range::default(),
            }],
            contract_sites: vec![],
            sql_constants: vec![],
            sql_execution_sites: vec![],
            string_constants: vec![],
        };
        vec![concrete, caller]
    };
    let out = resolve_edges(&files);
    let calls: Vec<_> = out
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .collect();
    assert!(
        calls
            .iter()
            .any(|e| e.dst == method_id("com.acme.UserServiceImpl", "save", 1)),
        "concrete field should resolve directly to impl"
    );
    assert!(
        !calls.iter().any(|e| e.reason == "di-resolved"),
        "concrete field should use receiver-bound, not di-resolved"
    );
}

#[test]
fn di_resolves_repository_interface() {
    let files = make_di_scenario(Some("repository"));
    let out = resolve_edges(&files);
    let calls: Vec<_> = out
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .collect();
    assert!(
        calls.iter().any(|e| {
            e.dst == method_id("com.acme.UserServiceImpl", "save", 1) && e.reason == "di-resolved"
        }),
        "@Repository impl should also be DI-resolved"
    );
}

#[test]
fn unresolved_ref_receiver_type_unknown() {
    let file = ParsedFile {
        file: "com/acme/Foo.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            type_def(NodeKind::Class, "com.acme.Foo"),
            method_def("com.acme.Foo", "go", &[], None),
        ],
        imports: vec![],
        reference_sites: vec![ref_site(
            "com.acme.Foo#go/0",
            method_id("com.acme.Foo", "go", 0),
            RefKind::Call,
            Some("unknownReceiver"),
            "doSomething",
            Some(0),
        )],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let out = resolve_edges(&[file]);
    assert_eq!(out.skipped, 1);
    assert_eq!(out.unresolved_refs.len(), 1);
    let r = &out.unresolved_refs[0];
    assert_eq!(r.reason, "receiver_type_unknown");
    assert_eq!(r.name, "doSomething");
    assert_eq!(r.receiver.as_deref(), Some("unknownReceiver"));
    assert_eq!(r.file, "com/acme/Foo.java");
}

#[test]
fn unresolved_ref_member_not_found() {
    let service = ParsedFile {
        file: "com/acme/MyService.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            type_def(NodeKind::Class, "com.acme.MyService"),
            method_def("com.acme.MyService", "knownMethod", &[], None),
        ],
        imports: vec![],
        reference_sites: vec![],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let caller = ParsedFile {
        file: "com/acme/Caller.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            type_def(NodeKind::Class, "com.acme.Caller"),
            method_def("com.acme.Caller", "run", &[], None),
            field_def("com.acme.Caller", "svc", "MyService"),
        ],
        imports: vec![],
        reference_sites: vec![ref_site(
            "com.acme.Caller#run/0",
            method_id("com.acme.Caller", "run", 0),
            RefKind::Call,
            Some("svc"),
            "missingMethod",
            Some(0),
        )],
        type_bindings: vec![TypeBinding {
            name: "svc".into(),
            raw_type: "MyService".into(),
            kind: BindingKind::Field,
            in_fqcn: "com.acme.Caller".into(),
            range: Range::default(),
        }],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let out = resolve_edges(&[service, caller]);
    assert_eq!(out.skipped, 1);
    let r = &out.unresolved_refs[0];
    assert_eq!(r.reason, "member_not_found");
    assert_eq!(
        r.resolved_receiver_type.as_deref(),
        Some("com.acme.MyService")
    );
}

#[test]
fn unresolved_ref_heritage_type_unknown() {
    let child = ParsedFile {
        file: "com/acme/Child.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![type_def(NodeKind::Class, "com.acme.Child")],
        imports: vec![],
        reference_sites: vec![heritage(
            "com.acme.Child",
            "MissingParent",
            RefKind::Extends,
        )],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let out = resolve_edges(&[child]);
    assert_eq!(out.skipped, 1);
    let r = &out.unresolved_refs[0];
    assert_eq!(r.reason, "heritage_type_unknown");
    assert_eq!(r.name, "MissingParent");
}

#[test]
fn callresult_factory_pattern_resolved() {
    let order_factory = ParsedFile {
        file: "com/acme/OrderFactory.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            type_def(NodeKind::Class, "com.acme.OrderFactory"),
            method_def("com.acme.OrderFactory", "create", &[], Some("Order")),
        ],
        imports: vec![],
        reference_sites: vec![],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let order = ParsedFile {
        file: "com/acme/Order.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            type_def(NodeKind::Class, "com.acme.Order"),
            method_def("com.acme.Order", "process", &[], None),
        ],
        imports: vec![],
        reference_sites: vec![],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let service = ParsedFile {
        file: "com/acme/OrderService.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            type_def(NodeKind::Class, "com.acme.OrderService"),
            method_def("com.acme.OrderService", "run", &[], None),
            field_def("com.acme.OrderService", "factory", "OrderFactory"),
        ],
        imports: vec![],
        reference_sites: vec![ref_site(
            "com.acme.OrderService#run/0",
            method_id("com.acme.OrderService", "run", 0),
            RefKind::Call,
            Some("order"),
            "process",
            Some(0),
        )],
        type_bindings: vec![
            TypeBinding {
                name: "factory".into(),
                raw_type: "OrderFactory".into(),
                kind: BindingKind::Field,
                in_fqcn: "com.acme.OrderService".into(),
                range: Range::default(),
            },
            TypeBinding {
                name: "order".into(),
                raw_type: "create".into(),
                kind: BindingKind::CallResult,
                in_fqcn: "com.acme.OrderService#run/0".into(),
                range: Range::default(),
            },
        ],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let out = resolve_edges(&[order_factory, order, service]);
    let calls: Vec<_> = out
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .collect();
    assert!(
        calls
            .iter()
            .any(|e| e.dst == method_id("com.acme.Order", "process", 0)),
        "factory CallResult should resolve order.process() to Order#process/0"
    );
    assert_eq!(
        out.skipped, 0,
        "no unresolved refs when factory pattern works"
    );
}

#[test]
fn callresult_factory_pattern_unresolved_when_return_type_absent() {
    let service = ParsedFile {
        file: "com/acme/OrderService.java".into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![
            type_def(NodeKind::Class, "com.acme.OrderService"),
            method_def("com.acme.OrderService", "run", &[], None),
        ],
        imports: vec![],
        reference_sites: vec![ref_site(
            "com.acme.OrderService#run/0",
            method_id("com.acme.OrderService", "run", 0),
            RefKind::Call,
            Some("order"),
            "process",
            Some(0),
        )],
        type_bindings: vec![TypeBinding {
            name: "order".into(),
            raw_type: "create".into(),
            kind: BindingKind::CallResult,
            in_fqcn: "com.acme.OrderService#run/0".into(),
            range: Range::default(),
        }],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let out = resolve_edges(&[service]);
    assert_eq!(out.skipped, 1);
    let r = &out.unresolved_refs[0];
    assert_eq!(r.reason, "receiver_type_unknown");
}

/// A module-level function def as the Python parser emits it: `NodeKind::Function`, `owner: None`,
/// `fqcn` = the module, empty `param_types`.
fn py_function_def(container: &str, name: &str, arity: u16) -> SymbolDef {
    SymbolDef {
        id: function_id(container, name, arity),
        kind: NodeKind::Function,
        fqcn: container.into(),
        name: name.into(),
        owner: None,
        range: Range::default(),
        modifiers: Vec::new(),
        param_types: Vec::new(),
        return_type: None,
        declared_type: None,
        framework_role: None,
        body_fingerprint: None,
        complexity: None,
        lang_meta: None,
    }
}

#[test]
fn python_free_function_call_resolves() {
    // Mirrors what the Python parser now emits: `NodeKind::Function` defs (empty `param_types`) and
    // a call ref attributed to the enclosing function `main`. Regression guard for the two Python
    // call-graph fixes (index registers Function-kind; parser attributes calls to the caller).
    let file = ParsedFile {
        file: "app.py".into(),
        language: String::new(),
        package: None,
        defs: vec![
            py_function_def("app", "helper", 1),
            py_function_def("app", "main", 0),
        ],
        imports: vec![],
        reference_sites: vec![ReferenceSite {
            name: "helper".into(),
            receiver: None,
            kind: RefKind::Call,
            arity: Some(1),
            range: Range::default(),
            in_fqcn: "app#main/0".into(),
            in_callable: function_id("app", "main", 0),
            arg_texts: vec![],
        }],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    };
    let out = resolve_edges(&[file]);
    assert!(
        out.edges.iter().any(|e| e.kind == EdgeKind::Calls
            && e.src == function_id("app", "main", 0)
            && e.dst == function_id("app", "helper", 1)),
        "expected CALLS edge main -> helper; calls = {:?}",
        out.edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls)
            .map(|e| (e.src.as_str(), e.dst.as_str()))
            .collect::<Vec<_>>()
    );
}

// ── Dynamic-URL folding (url_parts → constants + {*} wildcards) ─────────────

fn empty_file(file: &str) -> ParsedFile {
    ParsedFile {
        file: file.into(),
        language: String::new(),
        package: Some("com.acme".into()),
        defs: vec![],
        imports: vec![],
        reference_sites: vec![],
        type_bindings: vec![],
        contract_sites: vec![],
        sql_constants: vec![],
        sql_execution_sites: vec![],
        string_constants: vec![],
    }
}

fn http_parts_site(in_callable: NodeId, parts: Vec<UrlPart>) -> ContractSite {
    ContractSite {
        kind: ContractKind::HttpCall,
        url_template: None,
        topic: None,
        http_method: Some("GET".into()),
        messaging_framework: None,
        url_parts: Some(parts),
        in_callable,
        range: Range::default(),
    }
}

fn resolve_with_constants(parsed: Vec<ParsedFile>) -> cih_resolve::ResolveOutput {
    let resolver = build_java_constant_resolver(&parsed);
    resolve_with_registry(
        &parsed,
        &default_registry(),
        ResolveOptions {
            repo_root: None,
            enable_xml_integrations: false,
            constant_resolver: Some(Box::new(resolver)),
        },
    )
}

#[test]
fn dynamic_url_parts_fold_constants_and_wildcards() {
    // Two-file fold: the constant lives in another file; the trailing `id`
    // identifier never resolves and wildcards its segment.
    let mut constants = empty_file("com/acme/Constants.java");
    constants.defs = vec![type_def(NodeKind::Class, "com.acme.Constants")];
    constants.string_constants = vec![StringConstant {
        const_name: "BASE".into(),
        owner_fqcn: "com.acme.Constants".into(),
        value: "/api/orders".into(),
        env_default: false,
        dynamic: false,
        range: Range::default(),
    }];

    let caller = method_id("com.acme.Client", "call", 1);
    let mut client = empty_file("com/acme/Client.java");
    client.imports = vec![import("com.acme.Constants")];
    client.contract_sites = vec![http_parts_site(
        caller.clone(),
        vec![
            UrlPart::ConstRef("Constants.BASE".into()),
            UrlPart::Lit("/".into()),
            UrlPart::ConstRef("id".into()),
        ],
    )];

    let out = resolve_with_constants(vec![constants, client]);
    let endpoint = external_endpoint_id("GET", "/api/orders/{*}");
    let node = out
        .nodes
        .iter()
        .find(|node| node.id == endpoint)
        .expect("folded wildcard endpoint");
    assert_eq!(
        node.props.as_ref().and_then(|p| p.get("dynamic")),
        Some(&serde_json::Value::Bool(true))
    );
    let edge = out
        .edges
        .iter()
        .find(|edge| edge.kind == EdgeKind::ExternalCall && edge.src == caller)
        .expect("ExternalCall edge");
    assert!(
        edge.confidence < 0.75,
        "dynamic endpoints carry a confidence discount, got {}",
        edge.confidence
    );
}

#[test]
fn same_class_constant_resolves_without_qualifier() {
    let caller = method_id("com.acme.Client", "call", 0);
    let mut client = empty_file("com/acme/Client.java");
    client.defs = vec![type_def(NodeKind::Class, "com.acme.Client")];
    client.string_constants = vec![StringConstant {
        const_name: "BASE".into(),
        owner_fqcn: "com.acme.Client".into(),
        value: "/api/items".into(),
        env_default: false,
        dynamic: false,
        range: Range::default(),
    }];
    client.contract_sites = vec![http_parts_site(
        caller,
        vec![
            UrlPart::ConstRef("BASE".into()),
            UrlPart::Lit("/all".into()),
        ],
    )];

    let out = resolve_with_constants(vec![client]);
    let endpoint = external_endpoint_id("GET", "/api/items/all");
    assert!(out.nodes.iter().any(|node| node.id == endpoint));
}

#[test]
fn all_wildcard_dynamic_url_is_skipped() {
    let caller = method_id("com.acme.Client", "call", 0);
    let mut client = empty_file("com/acme/Client.java");
    client.contract_sites = vec![
        http_parts_site(caller.clone(), vec![UrlPart::Dynamic]),
        http_parts_site(
            caller,
            vec![UrlPart::ConstRef("nope".into()), UrlPart::Dynamic],
        ),
    ];

    let out = resolve_with_constants(vec![client]);
    assert!(
        !out.nodes
            .iter()
            .any(|node| node.kind == NodeKind::ExternalEndpoint),
        "uninformative all-wildcard URLs must not become endpoints"
    );
}

#[test]
fn dynamic_topic_folds_only_to_full_literal() {
    let caller = method_id("com.acme.Producer", "send", 0);
    let mut producer = empty_file("com/acme/Producer.java");
    producer.defs = vec![type_def(NodeKind::Class, "com.acme.Producer")];
    producer.string_constants = vec![StringConstant {
        const_name: "TOPIC".into(),
        owner_fqcn: "com.acme.Producer".into(),
        value: "orders.created".into(),
        env_default: false,
        dynamic: false,
        range: Range::default(),
    }];
    producer.contract_sites = vec![
        ContractSite {
            kind: ContractKind::EventPublish,
            url_template: None,
            topic: None,
            http_method: None,
            messaging_framework: Some(MessagingFramework::Kafka),
            url_parts: Some(vec![UrlPart::ConstRef("TOPIC".into())]),
            in_callable: caller.clone(),
            range: Range::default(),
        },
        ContractSite {
            kind: ContractKind::EventPublish,
            url_template: None,
            topic: None,
            http_method: None,
            messaging_framework: Some(MessagingFramework::Kafka),
            url_parts: Some(vec![UrlPart::Lit("orders.".into()), UrlPart::Dynamic]),
            in_callable: caller,
            range: Range::default(),
        },
    ];

    let out = resolve_with_constants(vec![producer]);
    let topics: Vec<&str> = out
        .nodes
        .iter()
        .filter(|node| node.kind == NodeKind::KafkaTopic)
        .map(|node| node.name.as_str())
        .collect();
    assert_eq!(topics, vec!["orders.created"]);
}

// ── Script-language constant folding (review-finding F2) ────────────────────

fn ts_const(file: &str, owner: &str, name: &str, value: &str, env_default: bool) -> ParsedFile {
    let mut pf = empty_file(file);
    pf.language = "typescript".into();
    pf.package = None;
    pf.string_constants = vec![StringConstant {
        const_name: name.into(),
        owner_fqcn: owner.into(),
        value: value.into(),
        dynamic: false,
        env_default,
        range: Range::default(),
    }];
    pf
}

fn ts_site_file(file: &str, in_callable: &str, parts: Vec<UrlPart>) -> ParsedFile {
    let mut pf = empty_file(file);
    pf.language = "typescript".into();
    pf.package = None;
    pf.contract_sites = vec![http_parts_site(NodeId::new(in_callable), parts)];
    pf
}

fn endpoint_paths(out: &cih_resolve::ResolveOutput) -> Vec<String> {
    out.nodes
        .iter()
        .filter(|n| n.kind == NodeKind::ExternalEndpoint)
        .filter_map(|n| {
            n.props
                .as_ref()?
                .get("path")
                .and_then(|p| p.as_str())
                .map(str::to_string)
        })
        .collect()
}

#[test]
fn ts_import_scoped_constant_resolves_across_files() {
    let constants = ts_const(
        "src/services/apiClient.ts",
        "src/services/apiClient",
        "API_BASE_URL",
        "/api/v1",
        true,
    );
    let mut site = ts_site_file(
        "src/services/svc.ts",
        "Function:src/services/svc#load/1",
        vec![
            UrlPart::ConstRef("API_BASE_URL".into()),
            UrlPart::Lit("/admin/x".into()),
        ],
    );
    site.imports = vec![import("./apiClient")];

    let out = resolve_with_constants(vec![constants, site]);
    assert_eq!(endpoint_paths(&out), vec!["/api/v1/admin/x"]);
    // env-default provenance surfaces on the endpoint.
    let endpoint = out
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::ExternalEndpoint)
        .unwrap();
    assert_eq!(
        endpoint
            .props
            .as_ref()
            .and_then(|p| p.get("base_source"))
            .and_then(|v| v.as_str()),
        Some("env_default")
    );
}

#[test]
fn ts_import_scoped_beats_repo_wide_ambiguity() {
    // Two same-named constants with different values; the site's file imports
    // exactly one of them — THAT one resolves, no wildcard, no guess.
    let imported = ts_const(
        "src/services/apiClient.ts",
        "src/services/apiClient",
        "API_BASE_URL",
        "/api/v1",
        false,
    );
    let unrelated = ts_const(
        "src/legacy/oldClient.ts",
        "src/legacy/oldClient",
        "API_BASE_URL",
        "/legacy",
        false,
    );
    let mut site = ts_site_file(
        "src/services/svc.ts",
        "Function:src/services/svc#load/1",
        vec![
            UrlPart::ConstRef("API_BASE_URL".into()),
            UrlPart::Lit("/admin/x".into()),
        ],
    );
    site.imports = vec![import("./apiClient")];

    let out = resolve_with_constants(vec![imported, unrelated, site]);
    assert_eq!(endpoint_paths(&out), vec!["/api/v1/admin/x"]);
}

#[test]
fn ts_unscoped_ambiguity_degrades_to_wildcard() {
    let a = ts_const("src/a.ts", "src/a", "API_BASE_URL", "/api/v1", false);
    let b = ts_const("src/b.ts", "src/b", "API_BASE_URL", "/legacy", false);
    // No import connects the site to either constant.
    let site = ts_site_file(
        "src/services/svc.ts",
        "Function:src/services/svc#load/1",
        vec![
            UrlPart::ConstRef("API_BASE_URL".into()),
            UrlPart::Lit("/admin/x".into()),
        ],
    );

    let out = resolve_with_constants(vec![a, b, site]);
    assert_eq!(endpoint_paths(&out), vec!["/{*}/admin/x"]);
}

#[test]
fn ts_unique_repo_wide_constant_resolves_without_import() {
    // Barrel re-export case: no direct import path, but the name is unique.
    let constant = ts_const(
        "src/config/constants.ts",
        "src/config/constants",
        "API_BASE_URL",
        "/api/v1",
        false,
    );
    let site = ts_site_file(
        "src/services/svc.ts",
        "Function:src/services/svc#load/1",
        vec![
            UrlPart::ConstRef("API_BASE_URL".into()),
            UrlPart::Lit("/admin/x".into()),
        ],
    );

    let out = resolve_with_constants(vec![constant, site]);
    assert_eq!(endpoint_paths(&out), vec!["/api/v1/admin/x"]);
}

#[test]
fn ts_module_scope_site_resolves_same_file_constant() {
    // Module-scope sites carry `File:`-derived owners with the extension;
    // the resolver strips it to reach the module owner scheme.
    let mut pf = ts_const(
        "src/services/apiClient.ts",
        "src/services/apiClient",
        "API_BASE_URL",
        "/api/v1",
        false,
    );
    pf.contract_sites = vec![http_parts_site(
        NodeId::new("File:src/services/apiClient.ts"),
        vec![
            UrlPart::ConstRef("API_BASE_URL".into()),
            UrlPart::Lit("/ping".into()),
        ],
    )];

    let out = resolve_with_constants(vec![pf]);
    assert_eq!(endpoint_paths(&out), vec!["/api/v1/ping"]);
}

#[test]
fn java_bare_name_never_uses_cross_file_fallback() {
    // Isolation pin: a java-language site with a bare ConstRef that misses
    // class scoping must NOT pick up a unique repo-wide constant — behavior
    // identical to before the fallback existed.
    let mut constants = empty_file("com/acme/Other.java");
    constants.defs = vec![type_def(NodeKind::Class, "com.acme.Other")];
    constants.string_constants = vec![StringConstant {
        const_name: "API_BASE_URL".into(),
        owner_fqcn: "com.acme.Other".into(),
        value: "/api/v1".into(),
        dynamic: false,
        env_default: false,
        range: Range::default(),
    }];

    let caller = method_id("com.acme.Client", "call", 1);
    let mut client = empty_file("com/acme/Client.java");
    client.language = "java".into();
    client.contract_sites = vec![http_parts_site(
        caller,
        vec![
            UrlPart::ConstRef("API_BASE_URL".into()),
            UrlPart::Lit("/admin/x".into()),
        ],
    )];

    let out = resolve_with_constants(vec![constants, client]);
    assert_eq!(endpoint_paths(&out), vec!["/{*}/admin/x"]);
}
