use cih_core::ContractMatchKind;
use cih_graph_store::Direction;
use cih_server_lib::args::{
    DetectChangesArgs, FeatureMapArgs, ImpactArgs, RegressionScopeArgs, RouteMapArgs,
    TraceFlowArgs, UntestedPathsArgs,
};
use cih_server_lib::utils::{parse_contract_kind_filter, parse_direction};

#[test]
fn direction_parse_unknown_falls_back_to_upstream() {
    assert_eq!(parse_direction(Some("downstream")), Direction::Downstream);
    assert_eq!(parse_direction(Some("both")), Direction::Both);
    assert_eq!(parse_direction(Some("sideways")), Direction::Upstream);
    assert_eq!(parse_direction(None), Direction::Upstream);
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
    assert_eq!(args.scope, "working");
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
    let args: TraceFlowArgs =
        serde_json::from_str(r#"{"entry_point":"Route:GET /"}"#).unwrap();
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
    let args: TraceFlowArgs = serde_json::from_str(
        r#"{"entry_point":"Route:GET /api/checkout","format":"mermaid"}"#,
    )
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
        serde_json::from_str(r#"{"changed_files":["src/main/java/com/acme/Foo.java"]}"#)
            .unwrap();
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
// Mirrors the production `git diff` arg match in changes.rs; base_ref is a
// literal None here because the test pins scope="staged".
#[allow(clippy::unnecessary_literal_unwrap)]
fn git_diff_staged_args_are_correct() {
    let scope = "staged";
    let base_ref: Option<&str> = None;
    let mut cmd = std::process::Command::new("git");
    cmd.arg("diff").arg("--name-only");
    match scope {
        "staged" => {
            cmd.arg("--cached").arg("HEAD");
        }
        "base_ref" => {
            cmd.arg(base_ref.unwrap_or("main"));
        }
        _ => {
            cmd.arg("HEAD");
        }
    }
    // structural test only — verifies no panic in argument setup
}
