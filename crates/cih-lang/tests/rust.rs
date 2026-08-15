use cih_core::{
    file_id, function_id, method_id, type_id, BindingKind, EdgeKind, NodeKind, RefKind,
};

#[test]
fn rust_impl_traits_imports_bindings_and_callable_measurement_are_extracted() {
    let source = r#"
use crate::models::{User, helper as aliased, nested::{Thing, *}};
use std::fmt::Debug;

trait Runner {
    fn required(&self, user: &User);
    fn defaulted(&self, user: &User) { self.required(user); }
}

struct Service;
impl Service {
    fn save(&self, user: User) {
        self.validate(&user);
        let local: User = user;
        helper(local);
        User::new();
        crate::util::helper();
    }

    fn validate(&self, user: &User) {}
}

fn helper(user: User) {}
fn with_closure() { let f = |value| value; f(1); }
"#;

    let unit = cih_lang::rust::parse::parse_rust_file("src/service.rs", source).unwrap();
    assert_eq!(unit.syntactic_callables, 7);
    assert_eq!(
        unit.nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Method)
            .count(),
        4
    );
    assert!(unit
        .nodes
        .iter()
        .any(|node| node.id == method_id("service::Service", "save", 1)));
    assert!(unit
        .nodes
        .iter()
        .any(|node| node.id == method_id("service::Runner", "required", 1)));
    assert!(unit
        .nodes
        .iter()
        .any(|node| node.id == function_id("service::helper", "helper", 1)));

    let helper = unit
        .parsed_file
        .defs
        .iter()
        .find(|definition| definition.name == "helper")
        .unwrap();
    assert_eq!(
        helper.fqcn, "service",
        "top-level NodeId stays legacy, def scope is module"
    );

    let imports = &unit.parsed_file.imports;
    assert!(imports
        .iter()
        .any(|import| import.raw == "crate::models::User"));
    assert!(imports.iter().any(|import| {
        import.raw == "crate::models::helper" && import.alias.as_deref() == Some("aliased")
    }));
    assert!(imports
        .iter()
        .any(|import| { import.raw == "crate::models::nested" && import.is_wildcard }));
    assert!(imports.iter().any(|import| import.raw == "std::fmt::Debug"));

    let save_scope = "service::Service#save/1";
    assert!(unit.parsed_file.type_bindings.iter().any(|binding| {
        binding.name == "user"
            && binding.raw_type == "User"
            && binding.kind == BindingKind::Param
            && binding.in_fqcn == save_scope
    }));
    assert!(unit.parsed_file.type_bindings.iter().any(|binding| {
        binding.name == "local"
            && binding.raw_type == "User"
            && binding.kind == BindingKind::Local
            && binding.in_fqcn == save_scope
    }));

    assert!(unit.parsed_file.reference_sites.iter().any(|site| {
        site.kind == RefKind::Call
            && site.receiver.as_deref() == Some("self")
            && site.name == "validate"
            && site.in_fqcn == save_scope
    }));
    assert!(unit
        .parsed_file
        .reference_sites
        .iter()
        .any(|site| { site.receiver.as_deref() == Some("User") && site.name == "new" }));
    assert!(unit
        .parsed_file
        .reference_sites
        .iter()
        .any(|site| { site.receiver.as_deref() == Some("crate::util") && site.name == "helper" }));
}

#[test]
fn rust_external_impl_emits_one_owner_placeholder_for_structure_edges() {
    let source = r#"
struct DirectionArg;

impl From<DirectionArg> for cih_graph_store::Direction {
    fn from(_direction: DirectionArg) -> Self { todo!() }
}

impl TryFrom<&str> for cih_graph_store::Direction {
    type Error = ();
    fn try_from(_direction: &str) -> Result<Self, Self::Error> { todo!() }
}
"#;

    let unit = cih_lang::rust::parse::parse_rust_file("src/args.rs", source).unwrap();
    let owner_id = type_id(NodeKind::Class, "cih_graph_store::Direction");

    let owners = unit
        .nodes
        .iter()
        .filter(|node| node.id == owner_id)
        .collect::<Vec<_>>();
    assert_eq!(owners.len(), 1);
    assert_eq!(
        owners[0]
            .props
            .as_ref()
            .and_then(|props| props.get("external_owner"))
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert!(unit.edges.iter().any(|edge| {
        edge.src == file_id("src/args.rs")
            && edge.dst == owner_id
            && edge.kind == EdgeKind::Contains
    }));
    for method in [
        method_id("cih_graph_store::Direction", "from", 1),
        method_id("cih_graph_store::Direction", "try_from", 1),
    ] {
        assert!(unit.nodes.iter().any(|node| node.id == method));
        assert!(unit.edges.iter().any(|edge| {
            edge.src == owner_id && edge.dst == method && edge.kind == EdgeKind::HasMethod
        }));
    }
}
