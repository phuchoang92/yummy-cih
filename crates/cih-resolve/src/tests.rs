use crate::default_registry;
use crate::index::ResolveIndex;
use crate::types::simple_of;
use cih_core::{
    constructor_id, field_id, method_id, type_id, BindingKind, NodeKind, ParsedFile, Range,
    RawImport, RefKind, ReferenceSite, SymbolDef, TypeBinding,
};

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
        alias: None,
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
        http_wrappers: Vec::new(),
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
        http_wrappers: Vec::new(),
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
        http_wrappers: Vec::new(),
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
        http_wrappers: Vec::new(),
    };
    vec![repo, service, controller, thing]
}

#[test]
fn resolve_type_uses_import_same_package_and_generics() {
    let idx = ResolveIndex::build(&workspace(), &default_registry());
    let f = "com/acme/OwnerController.java";
    assert_eq!(
        idx.resolve_type("List", f).as_deref(),
        Some("java.util.List")
    );
    assert_eq!(
        idx.resolve_type("Thing", f).as_deref(),
        Some("com.other.Thing")
    );
    assert_eq!(
        idx.resolve_type("Owner", f).as_deref(),
        Some("com.acme.Owner")
    );
    assert_eq!(
        idx.resolve_type("List<Owner>", f).as_deref(),
        Some("java.util.List")
    );
    assert_eq!(idx.resolve_type("Nope", f), None);
}

#[test]
fn find_member_matches_overload_by_arity() {
    let idx = ResolveIndex::build(&workspace(), &default_registry());
    assert_eq!(
        idx.find_member("com.acme.OwnerService", "save", Some(1)),
        Some(method_id("com.acme.OwnerService", "save", 1))
    );
    assert_eq!(
        idx.find_member("com.acme.OwnerService", "save", Some(2)),
        Some(method_id("com.acme.OwnerService", "save", 2))
    );
    assert_eq!(
        idx.find_member("com.acme.OwnerService", "missing", Some(0)),
        None
    );
}

#[test]
fn receiver_type_param_field_and_this() {
    let idx = ResolveIndex::build(&workspace(), &default_registry());
    let scope = "com.acme.OwnerController#handle/1";
    assert_eq!(
        idx.receiver_type(scope, "svc").as_deref(),
        Some("com.acme.OwnerService")
    );
    assert_eq!(
        idx.receiver_type(scope, "service").as_deref(),
        Some("com.acme.OwnerService")
    );
    assert_eq!(
        idx.receiver_type(scope, "this").as_deref(),
        Some("com.acme.OwnerController")
    );
    assert_eq!(idx.receiver_type(scope, "unknown"), None);
}

#[test]
fn local_param_shadows_field() {
    let mut files = workspace();
    files[2].type_bindings.push(binding(
        "service",
        "Owner",
        BindingKind::Local,
        "com.acme.OwnerController#handle/1",
        6,
    ));
    let idx = ResolveIndex::build(&files, &default_registry());
    assert_eq!(
        idx.receiver_type("com.acme.OwnerController#handle/1", "service")
            .as_deref(),
        Some("com.acme.Owner"),
        "a local must shadow the field of the same name"
    );
}

#[test]
fn heritage_and_inherited_member_lookup() {
    let idx = ResolveIndex::build(&workspace(), &default_registry());
    assert_eq!(idx.supertypes("com.acme.OwnerService"), ["com.acme.Repo"]);
    assert_eq!(idx.implementors("com.acme.Repo"), ["com.acme.OwnerService"]);
    assert_eq!(
        idx.find_member("com.acme.OwnerService", "findAll", Some(0)),
        None
    );
    assert_eq!(
        idx.find_member_in_hierarchy("com.acme.OwnerService", "findAll", Some(0)),
        Some(method_id("com.acme.Repo", "findAll", 0))
    );
}
