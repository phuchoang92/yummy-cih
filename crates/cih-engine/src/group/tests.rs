use super::*;

#[test]
fn normalizes_route_variables() {
    assert_eq!(
        normalize_contract_path("/api/orders/{id}?debug=true"),
        "/api/orders/{*}"
    );
    assert_eq!(
        normalize_contract_path("http://orders.local/api/orders/:id"),
        "/api/orders/{*}"
    );
}

#[test]
fn matches_http_provider_and_consumer_across_repos() {
    let provider = RepoContracts {
        routes: vec![RouteContract {
            repo: "orders".into(),
            id: "Route:GET /api/orders/{id}".into(),
            method: "GET".into(),
            path: "/api/orders/{id}".into(),
        }],
        ..RepoContracts::default()
    };
    let consumer = RepoContracts {
        endpoints: vec![EndpointContract {
            repo: "checkout".into(),
            id: "ExternalEndpoint:GET:/api/orders/42".into(),
            method: "GET".into(),
            path: "/api/orders/:id".into(),
        }],
        ..RepoContracts::default()
    };

    let matches = match_contracts(&[provider, consumer]);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].kind, ContractMatchKind::HttpRoute);
    assert_eq!(matches[0].provider_repo, "orders");
    assert_eq!(matches[0].consumer_repo, "checkout");
}
