use cih_core::ContractMatchKind;
use cih_graph_store::Direction;
use cih_server::args::{
    DetectChangesArgs, DiffScope, DirectionArg, FeatureMapArgs, ImpactArgs, RegressionScopeArgs,
    RouteMapArgs, TraceFlowArgs, UntestedPathsArgs,
};
use cih_server::utils::parse_contract_kind_filter;

#[test]
fn direction_arg_is_typed_and_rejects_unknown_values() {
    // Valid values deserialize; omitted defaults to upstream.
    let args: ImpactArgs =
        serde_json::from_str(r#"{"name":"X","direction":"downstream"}"#).unwrap();
    assert_eq!(args.direction, DirectionArg::Downstream);
    let args: ImpactArgs = serde_json::from_str(r#"{"name":"X"}"#).unwrap();
    assert_eq!(args.direction, DirectionArg::Upstream);
    assert_eq!(Direction::from(DirectionArg::Both), Direction::Both);
    assert_eq!(
        Direction::from(DirectionArg::Downstream),
        Direction::Downstream
    );
    assert_eq!(Direction::from(DirectionArg::Upstream), Direction::Upstream);
    // A typo is an error now — it used to silently run `upstream`.
    assert!(serde_json::from_str::<ImpactArgs>(r#"{"name":"X","direction":"sideways"}"#).is_err());
    assert!(serde_json::from_str::<ImpactArgs>(r#"{"name":"X","direction":""}"#).is_err());
}

#[test]
fn diff_scope_is_typed_and_rejects_unknown_values() {
    for (raw, want) in [
        ("working", DiffScope::Working),
        ("staged", DiffScope::Staged),
        ("base_ref", DiffScope::BaseRef),
    ] {
        let args: DetectChangesArgs =
            serde_json::from_str(&format!(r#"{{"scope":"{raw}"}}"#)).unwrap();
        assert_eq!(args.scope, want);
    }
    // A typo is an error now — it used to silently run the `working` diff.
    assert!(serde_json::from_str::<DetectChangesArgs>(r#"{"scope":"stagd"}"#).is_err());
    assert!(serde_json::from_str::<DetectChangesArgs>(r#"{}"#).is_err());
}

#[test]
fn route_map_args_default_limit_is_zero() {
    let args: RouteMapArgs = serde_json::from_str("{}").unwrap();
    assert!(args.prefix.is_empty());
    assert_eq!(args.limit, 0);
}

#[test]
fn detect_changes_args_defaults() {
    let args: DetectChangesArgs = serde_json::from_str(r#"{"scope":"working"}"#).unwrap();
    assert_eq!(args.scope, DiffScope::Working);
    assert!(args.base_ref.is_empty());
    assert!(args.repo.is_empty());
}

#[test]
fn contract_kind_filter_accepts_aliases() {
    assert_eq!(parse_contract_kind_filter(None).unwrap(), None);
    assert_eq!(
        parse_contract_kind_filter(Some("http")).unwrap(),
        Some(ContractMatchKind::HttpRoute)
    );
    assert_eq!(
        parse_contract_kind_filter(Some("kafka_topic")).unwrap(),
        Some(ContractMatchKind::KafkaTopic)
    );
    assert_eq!(
        parse_contract_kind_filter(Some("spring-event")).unwrap(),
        Some(ContractMatchKind::SpringEvent)
    );
    assert!(parse_contract_kind_filter(Some("queue")).is_err());
}

#[test]
fn trace_flow_args_defaults() {
    let args: TraceFlowArgs = serde_json::from_str(r#"{"entry_point":"Route:GET /"}"#).unwrap();
    assert_eq!(args.entry_point, "Route:GET /");
    assert_eq!(args.max_depth, 0);
    assert!(args.format.is_empty());
}

#[test]
fn impact_args_accepts_format_diagram() {
    let args: ImpactArgs =
        serde_json::from_str(r#"{"name":"OrderService","format":"diagram"}"#).unwrap();
    assert_eq!(args.name, "OrderService");
    assert_eq!(args.format, "diagram");
}

#[test]
fn trace_flow_args_accepts_format_mermaid() {
    let args: TraceFlowArgs =
        serde_json::from_str(r#"{"entry_point":"Route:GET /api/checkout","format":"mermaid"}"#)
            .unwrap();
    assert_eq!(args.entry_point, "Route:GET /api/checkout");
    assert_eq!(args.format, "mermaid");
}

#[test]
fn feature_map_args_defaults() {
    let args: FeatureMapArgs = serde_json::from_str(r#"{"query":"checkout"}"#).unwrap();
    assert_eq!(args.query, "checkout");
    assert_eq!(args.limit, 0);
}

#[test]
fn regression_scope_args_parses_file_list() {
    let args: RegressionScopeArgs =
        serde_json::from_str(r#"{"changed_files":["src/main/java/com/acme/Foo.java"]}"#).unwrap();
    assert_eq!(args.changed_files.len(), 1);
    assert_eq!(args.changed_files[0], "src/main/java/com/acme/Foo.java");
}

#[test]
fn untested_paths_args_defaults() {
    let args: UntestedPathsArgs =
        serde_json::from_str(r#"{"module_prefix":"src/main/java/com/acme"}"#).unwrap();
    assert_eq!(args.module_prefix, "src/main/java/com/acme");
    assert_eq!(args.limit, 0);
}

#[test]
fn index_repo_args_graph_key_defaults_empty() {
    let args: cih_server::args::IndexRepoArgs =
        serde_json::from_str(r#"{"repo_path":"/tmp/x"}"#).unwrap();
    assert!(args.graph_key.is_empty());
    let args: cih_server::args::IndexRepoArgs =
        serde_json::from_str(r#"{"repo_path":"/tmp/x","graph_key":"svc"}"#).unwrap();
    assert_eq!(args.graph_key, "svc");
}

#[test]
fn trace_flow_x_args_repo_defaults_empty() {
    let args: cih_server::args::TraceFlowXArgs =
        serde_json::from_str(r#"{"entry_point":"Route:GET /x","group":"g"}"#).unwrap();
    assert!(args.repo.is_empty());
    assert_eq!(args.max_depth, 0);
}
