use super::*;
use cih_core::{NodeKind, Range};

fn prop<'a>(node: &'a Node, key: &str) -> Option<&'a str> {
    node.props.as_ref()?.get(key)?.as_str()
}

fn integration_route(name: &str, source: &str, extra: serde_json::Value) -> Node {
    let mut props = serde_json::json!({ "source": source });
    if let (Some(obj), Some(ex)) = (props.as_object_mut(), extra.as_object()) {
        for (k, v) in ex {
            obj.insert(k.clone(), v.clone());
        }
    }
    Node {
        id: NodeId::new(format!("IntegrationRoute:{source}:{name}")),
        kind: NodeKind::IntegrationRoute,
        name: name.to_string(),
        qualified_name: extra
            .get("class")
            .and_then(|v| v.as_str())
            .map(String::from),
        file: "beans.xml".to_string(),
        range: Range::default(),
        props: Some(props),
    }
}

fn route_node(method: &str, path: &str, handler: &str) -> Node {
    Node {
        id: NodeId::new(format!("Route:{method} {path}")),
        kind: NodeKind::Route,
        name: format!("{method} {path}"),
        qualified_name: Some(format!("{method} {path}")),
        file: "com/acme/Endpoint.java".to_string(),
        range: Range::default(),
        props: Some(serde_json::json!({
            "httpMethod": method,
            "path": path,
            "handler": handler,
        })),
    }
}

fn handles_route_edge(handler: &str, method: &str, path: &str) -> Edge {
    Edge {
        src: NodeId::new(format!("Method:{handler}")),
        dst: NodeId::new(format!("Route:{method} {path}")),
        kind: EdgeKind::HandlesRoute,
        confidence: 1.0,
        reason: String::new(),
        props: None,
    }
}

/// A `<jaxrs:server address>` + its referenced bean, mirroring the parsed XML nodes.
fn server_and_bean(address: &str, bean_id: &str, class: &str) -> Vec<Node> {
    vec![
        integration_route(
            address,
            "cxf_jaxrs_server",
            serde_json::json!({ "address": address, "bean_id": bean_id, "beans": [bean_id] }),
        ),
        integration_route(bean_id, "spring_xml", serde_json::json!({ "class": class })),
    ]
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("cih-cxf-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Re-home a synthetic node to a specific repo-relative file (the default
/// helpers hardcode `beans.xml`, which puts everything in one "bundle").
fn at_file(mut node: Node, file: &str) -> Node {
    node.file = file.to_string();
    node
}

fn interface_node(fqcn: &str) -> Node {
    Node {
        id: NodeId::new(format!("Interface:{fqcn}")),
        kind: NodeKind::Interface,
        name: crate::di_xml::simple_name(fqcn).to_string(),
        qualified_name: Some(fqcn.to_string()),
        file: "com/acme/Api.java".to_string(),
        range: Range::default(),
        props: None,
    }
}

/// A heritage edge as `emit_heritage_edges` produces it: subtype → supertype.
fn heritage_edge(kind: EdgeKind, sub_id: &str, super_id: &str) -> Edge {
    Edge {
        src: NodeId::new(sub_id),
        dst: NodeId::new(super_id),
        kind,
        confidence: 1.0,
        reason: "heritage".to_string(),
        props: None,
    }
}

/// One OCB-style bundle: a `<jaxrs:server>` + bean in `beans_rest.xml` and a
/// whiteboard servlet pattern in `beans_rest_web_servlets.xml`, all under
/// `<bundle_dir>/resources/META-INF/spring/`.
fn bundle(bundle_dir: &str, pattern: &str, address: &str, bean_id: &str, class: &str) -> Vec<Node> {
    let spring_dir = format!("{bundle_dir}/resources/META-INF/spring");
    let mut nodes: Vec<Node> = server_and_bean(address, bean_id, class)
        .into_iter()
        .map(|n| {
            let file = format!("{spring_dir}/beans_rest.xml");
            at_file(n, &file)
        })
        .collect();
    nodes.push(at_file(
        integration_route(
            pattern,
            "osgi_servlet",
            serde_json::json!({ "servlet_pattern": pattern }),
        ),
        &format!("{spring_dir}/beans_rest_web_servlets.xml"),
    ));
    nodes
}


#[test]
fn stitch_interface_handler_via_impl_class() {
    // OCB shape: @Path lives on the interface in the -api bundle; the
    // jaxrs:server bean is the impl. The route's handler is the interface.
    let dir = temp_dir("iface");
    let mut nodes = server_and_bean("/v1", "restImpl", "com.acme.RestImpl");
    nodes.push(class_node("com.acme.RestImpl"));
    nodes.push(interface_node("com.acme.api.RestService"));
    let handler = "com.acme.api.RestService#op/1";
    nodes.push(route_node("GET", "/op", handler));
    let mut edges = vec![
        handles_route_edge(handler, "GET", "/op"),
        heritage_edge(
            EdgeKind::Implements,
            "Class:com.acme.RestImpl",
            "Interface:com.acme.api.RestService",
        ),
    ];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    assert_eq!(prop(route, "path"), Some("/v1/op"));
    assert_eq!(prop(route, "local_path"), Some("/op"));
    let hr = edges
        .iter()
        .find(|e| e.kind == EdgeKind::HandlesRoute)
        .unwrap();
    assert_eq!(hr.dst.as_str(), "Route:GET /v1/op");
    assert!(edges
        .iter()
        .any(|e| e.kind == EdgeKind::IntegrationLink && e.reason == "cxf-jaxrs-prefix"));
}

#[test]
fn stitch_interface_fallback_transitive_extends() {
    // Impl implements A, interface A extends B, annotations on B.
    let dir = temp_dir("iface-trans");
    let mut nodes = server_and_bean("/v1", "restImpl", "com.acme.RestImpl");
    nodes.push(class_node("com.acme.RestImpl"));
    nodes.push(interface_node("com.acme.api.A"));
    nodes.push(interface_node("com.acme.api.B"));
    let handler = "com.acme.api.B#op/0";
    nodes.push(route_node("GET", "/op", handler));
    let mut edges = vec![
        handles_route_edge(handler, "GET", "/op"),
        heritage_edge(
            EdgeKind::Implements,
            "Class:com.acme.RestImpl",
            "Interface:com.acme.api.A",
        ),
        heritage_edge(
            EdgeKind::Extends,
            "Interface:com.acme.api.A",
            "Interface:com.acme.api.B",
        ),
    ];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    assert_eq!(prop(route, "path"), Some("/v1/op"));
}

#[test]
fn stitch_exact_impl_match_beats_interface_fallback() {
    // Two servers: one names the handler's class exactly, the other only
    // reaches it via the interface set. The exact one must win.
    let dir = temp_dir("exact-wins");
    let mut nodes = server_and_bean("/direct", "implBean", "com.acme.RestImpl");
    nodes.extend(server_and_bean("/other", "otherBean", "com.acme.OtherImpl"));
    nodes.push(class_node("com.acme.RestImpl"));
    nodes.push(class_node("com.acme.OtherImpl"));
    nodes.push(interface_node("com.acme.api.RestService"));
    // OtherImpl implements the interface; the route handler is the IMPL
    // class RestImpl, so the /direct server matches exactly.
    let handler = "com.acme.RestImpl#op/0";
    nodes.push(route_node("GET", "/op", handler));
    let mut edges = vec![
        handles_route_edge(handler, "GET", "/op"),
        heritage_edge(
            EdgeKind::Implements,
            "Class:com.acme.RestImpl",
            "Interface:com.acme.api.RestService",
        ),
        heritage_edge(
            EdgeKind::Implements,
            "Class:com.acme.OtherImpl",
            "Interface:com.acme.api.RestService",
        ),
    ];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    assert_eq!(prop(route, "path"), Some("/direct/op"));
}

#[test]
fn stitch_interface_handler_without_heritage_is_noop() {
    let dir = temp_dir("iface-none");
    let mut nodes = server_and_bean("/v1", "restImpl", "com.acme.RestImpl");
    nodes.push(class_node("com.acme.RestImpl"));
    nodes.push(interface_node("com.acme.api.Unrelated"));
    let handler = "com.acme.api.Unrelated#op/0";
    nodes.push(route_node("GET", "/op", handler));
    let mut edges = vec![handles_route_edge(handler, "GET", "/op")];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    assert_eq!(prop(route, "path"), Some("/op"));
    assert!(prop(route, "local_path").is_none());
    assert!(!edges
        .iter()
        .any(|e| e.kind == EdgeKind::IntegrationLink && e.reason == "cxf-jaxrs-prefix"));
}


#[test]
fn stitch_dual_servers_clone_route_per_address() {
    // Secured /v1 and non-secured /ns/v1 servers, two impl beans, one
    // annotated interface: one handler must yield TWO routes.
    let dir = temp_dir("dual");
    let mut nodes = server_and_bean("/v1", "securedImpl", "com.acme.SecuredImpl");
    nodes.extend(server_and_bean("/ns/v1", "nonSecuredImpl", "com.acme.NonSecuredImpl"));
    nodes.push(class_node("com.acme.SecuredImpl"));
    nodes.push(class_node("com.acme.NonSecuredImpl"));
    nodes.push(interface_node("com.acme.api.RemitService"));
    let handler = "com.acme.api.RemitService#send/1";
    nodes.push(route_node("POST", "/send", handler));
    let mut edges = vec![
        handles_route_edge(handler, "POST", "/send"),
        heritage_edge(
            EdgeKind::Implements,
            "Class:com.acme.SecuredImpl",
            "Interface:com.acme.api.RemitService",
        ),
        heritage_edge(
            EdgeKind::Implements,
            "Class:com.acme.NonSecuredImpl",
            "Interface:com.acme.api.RemitService",
        ),
    ];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let routes: Vec<&Node> = nodes.iter().filter(|n| n.kind == NodeKind::Route).collect();
    assert_eq!(routes.len(), 2, "one route per server address");
    let mut paths: Vec<&str> = routes.iter().filter_map(|n| prop(n, "path")).collect();
    paths.sort();
    assert_eq!(paths, vec!["/ns/v1/send", "/v1/send"]);
    for r in &routes {
        assert_eq!(prop(r, "local_path"), Some("/send"));
        assert_eq!(prop(r, "handler"), Some(handler));
    }

    let hr: Vec<&Edge> = edges
        .iter()
        .filter(|e| e.kind == EdgeKind::HandlesRoute)
        .collect();
    assert_eq!(hr.len(), 2, "HANDLES_ROUTE duplicated onto the clone");
    assert_eq!(hr[0].src, hr[1].src, "same handler method");
    let mut hr_dsts: Vec<&str> = hr.iter().map(|e| e.dst.as_str()).collect();
    hr_dsts.sort();
    assert_eq!(hr_dsts, vec!["Route:POST /ns/v1/send", "Route:POST /v1/send"]);

    // Each route has a provenance link from its own server.
    let links: Vec<&Edge> = edges
        .iter()
        .filter(|e| e.kind == EdgeKind::IntegrationLink && e.reason == "cxf-jaxrs-prefix")
        .collect();
    assert_eq!(links.len(), 2);
    assert_ne!(links[0].src, links[1].src, "distinct server nodes");
}

#[test]
fn stitch_dual_servers_same_resulting_path_dedups() {
    // Two servers with the SAME address referencing the two impls: paths
    // collide, so no clone is made.
    let dir = temp_dir("dual-same");
    let mut nodes = server_and_bean("/v1", "securedImpl", "com.acme.SecuredImpl");
    nodes.extend(server_and_bean("/v1", "nonSecuredImpl", "com.acme.NonSecuredImpl"));
    nodes.push(class_node("com.acme.SecuredImpl"));
    nodes.push(class_node("com.acme.NonSecuredImpl"));
    nodes.push(interface_node("com.acme.api.RemitService"));
    let handler = "com.acme.api.RemitService#send/1";
    nodes.push(route_node("POST", "/send", handler));
    let mut edges = vec![
        handles_route_edge(handler, "POST", "/send"),
        heritage_edge(
            EdgeKind::Implements,
            "Class:com.acme.SecuredImpl",
            "Interface:com.acme.api.RemitService",
        ),
        heritage_edge(
            EdgeKind::Implements,
            "Class:com.acme.NonSecuredImpl",
            "Interface:com.acme.api.RemitService",
        ),
    ];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let routes: Vec<&Node> = nodes.iter().filter(|n| n.kind == NodeKind::Route).collect();
    assert_eq!(routes.len(), 1);
    assert_eq!(prop(routes[0], "path"), Some("/v1/send"));
    assert_eq!(
        edges
            .iter()
            .filter(|e| e.kind == EdgeKind::HandlesRoute)
            .count(),
        1
    );
}

#[test]
fn stitch_clone_skipped_when_id_already_exists() {
    // A pre-existing route already owns the would-be clone id: no duplicate node.
    let dir = temp_dir("dual-collide");
    let mut nodes = server_and_bean("/v1", "securedImpl", "com.acme.SecuredImpl");
    nodes.extend(server_and_bean("/ns/v1", "nonSecuredImpl", "com.acme.NonSecuredImpl"));
    nodes.push(class_node("com.acme.SecuredImpl"));
    nodes.push(class_node("com.acme.NonSecuredImpl"));
    nodes.push(interface_node("com.acme.api.RemitService"));
    let handler = "com.acme.api.RemitService#send/1";
    nodes.push(route_node("POST", "/send", handler));
    // Unrelated pre-existing route occupying the clone's id.
    nodes.push(route_node("POST", "/ns/v1/send", "com.acme.Other#send/1"));
    let mut edges = vec![
        handles_route_edge(handler, "POST", "/send"),
        heritage_edge(
            EdgeKind::Implements,
            "Class:com.acme.SecuredImpl",
            "Interface:com.acme.api.RemitService",
        ),
        heritage_edge(
            EdgeKind::Implements,
            "Class:com.acme.NonSecuredImpl",
            "Interface:com.acme.api.RemitService",
        ),
    ];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let ids: Vec<&str> = nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Route)
        .map(|n| n.id.as_str())
        .collect();
    let unique: std::collections::HashSet<&&str> = ids.iter().collect();
    assert_eq!(ids.len(), unique.len(), "no duplicate route ids: {ids:?}");
}

#[test]
fn dual_server_bundle_full_ocb_shape() {
    // The full OCB remittance shape: whiteboard /rest/remittance/* + a
    // secured and a non-secured server in one bundle, interface handler.
    let dir = temp_dir("ocb");
    let spring = "custom-remittance/resources/META-INF/spring";
    let mut nodes: Vec<Node> = server_and_bean("/v1", "securedImpl", "com.vpb.RemitImpl")
        .into_iter()
        .map(|n| at_file(n, &format!("{spring}/beans_rest.xml")))
        .collect();
    nodes.extend(
        server_and_bean("/ns/v1", "nsImpl", "com.vpb.NsRemitImpl")
            .into_iter()
            .map(|n| at_file(n, &format!("{spring}/beans_rest.xml"))),
    );
    nodes.push(at_file(
        integration_route(
            "/rest/remittance/*",
            "osgi_servlet",
            serde_json::json!({ "servlet_pattern": "/rest/remittance/*" }),
        ),
        &format!("{spring}/beans_rest_web_servlets.xml"),
    ));
    nodes.push(class_node("com.vpb.RemitImpl"));
    nodes.push(class_node("com.vpb.NsRemitImpl"));
    nodes.push(interface_node("com.vpb.api.RemittanceService"));
    let handler = "com.vpb.api.RemittanceService#getBeneficiaries/0";
    nodes.push(route_node("GET", "/beneficiaries", handler));
    let mut edges = vec![
        handles_route_edge(handler, "GET", "/beneficiaries"),
        heritage_edge(
            EdgeKind::Implements,
            "Class:com.vpb.RemitImpl",
            "Interface:com.vpb.api.RemittanceService",
        ),
        heritage_edge(
            EdgeKind::Implements,
            "Class:com.vpb.NsRemitImpl",
            "Interface:com.vpb.api.RemittanceService",
        ),
    ];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let routes: Vec<&Node> = nodes.iter().filter(|n| n.kind == NodeKind::Route).collect();
    let mut paths: Vec<&str> = routes.iter().filter_map(|n| prop(n, "path")).collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "/rest/remittance/ns/v1/beneficiaries",
            "/rest/remittance/v1/beneficiaries"
        ]
    );
    assert!(routes
        .iter()
        .all(|n| prop(n, "servlet_prefix_source") == Some("osgi_whiteboard")));
}

#[test]
fn per_bundle_servlet_prefix_selected_by_directory() {
    let dir = temp_dir("bundles");
    let mut nodes = bundle("custom-a", "/rest/a/*", "/v1", "aImpl", "com.acme.a.AImpl");
    nodes.extend(bundle("custom-b", "/rest/b/*", "/v1", "bImpl", "com.acme.b.BImpl"));
    nodes.push(route_node("GET", "/x", "com.acme.a.AImpl#x/0"));
    nodes.push(route_node("GET", "/y", "com.acme.b.BImpl#y/0"));
    let mut edges = vec![
        handles_route_edge("com.acme.a.AImpl#x/0", "GET", "/x"),
        handles_route_edge("com.acme.b.BImpl#y/0", "GET", "/y"),
    ];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let paths: Vec<&str> = nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Route)
        .filter_map(|n| prop(n, "path"))
        .collect();
    assert!(paths.contains(&"/rest/a/v1/x"), "paths: {paths:?}");
    assert!(paths.contains(&"/rest/b/v1/y"), "paths: {paths:?}");
    assert!(nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Route)
        .all(|n| prop(n, "servlet_prefix_source") == Some("osgi_whiteboard")));
}

#[test]
fn single_osgi_servlet_applies_across_directories() {
    // A lone whiteboard pattern still applies repo-wide even when it shares
    // no directory with the server (single-bundle repos, root-level XML).
    let dir = temp_dir("lone");
    let mut nodes = server_and_bean("/v1", "impl", "com.acme.Impl")
        .into_iter()
        .map(|n| at_file(n, "app/config/beans_rest.xml"))
        .collect::<Vec<_>>();
    nodes.push(at_file(
        integration_route(
            "/rest/*",
            "osgi_servlet",
            serde_json::json!({ "servlet_pattern": "/rest/*" }),
        ),
        "web/servlets.xml",
    ));
    nodes.push(route_node("GET", "/x", "com.acme.Impl#x/0"));
    let mut edges = vec![handles_route_edge("com.acme.Impl#x/0", "GET", "/x")];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    assert_eq!(prop(route, "path"), Some("/rest/v1/x"));
    assert_eq!(
        prop(route, "servlet_prefix_source"),
        Some("osgi_whiteboard")
    );
}

#[test]
fn multiple_unrelated_osgi_servlets_do_not_cross_apply() {
    // Two bundles declare patterns; a server in a THIRD bundle must not
    // inherit either one (previously: first-node-wins repo-wide).
    let dir = temp_dir("unrelated");
    let mut nodes = bundle("custom-a", "/rest/a/*", "/v1", "aImpl", "com.acme.a.AImpl");
    nodes.extend(bundle("custom-b", "/rest/b/*", "/v1", "bImpl", "com.acme.b.BImpl"));
    nodes.extend(
        server_and_bean("/v1", "cImpl", "com.acme.c.CImpl")
            .into_iter()
            .map(|n| at_file(n, "custom-c/resources/META-INF/spring/beans_rest.xml")),
    );
    nodes.push(route_node("GET", "/z", "com.acme.c.CImpl#z/0"));
    let mut edges = vec![handles_route_edge("com.acme.c.CImpl#z/0", "GET", "/z")];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let route = nodes
        .iter()
        .find(|n| n.kind == NodeKind::Route && prop(n, "handler") == Some("com.acme.c.CImpl#z/0"))
        .unwrap();
    assert_eq!(prop(route, "path"), Some("/v1/z"));
    assert_eq!(prop(route, "servlet_prefix_source"), Some("none"));
}

#[test]
fn config_override_beats_per_bundle_pattern() {
    let dir = temp_dir("cfgwins");
    let mut nodes = bundle("custom-a", "/rest/a/*", "/v1", "aImpl", "com.acme.a.AImpl");
    nodes.push(route_node("GET", "/x", "com.acme.a.AImpl#x/0"));
    let mut edges = vec![handles_route_edge("com.acme.a.AImpl#x/0", "GET", "/x")];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, Some("/api"));
    std::fs::remove_dir_all(&dir).ok();

    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    assert_eq!(prop(route, "path"), Some("/api/v1/x"));
    assert_eq!(prop(route, "servlet_prefix_source"), Some("config"));
}

#[test]
fn servlet_prefix_tie_breaks_deterministically() {
    // Two patterns equidistant from the server (same shared directory
    // depth): the lexicographically-first shortest file wins.
    let dir = temp_dir("ties");
    let mut nodes = server_and_bean("/v1", "impl", "com.acme.Impl")
        .into_iter()
        .map(|n| at_file(n, "app/spring/beans_rest.xml"))
        .collect::<Vec<_>>();
    for (file, pattern) in [
        ("app/z/servlets.xml", "/rest/z/*"),
        ("app/a/servlets.xml", "/rest/a/*"),
    ] {
        nodes.push(at_file(
            integration_route(
                pattern,
                "osgi_servlet",
                serde_json::json!({ "servlet_pattern": pattern }),
            ),
            file,
        ));
    }
    nodes.push(route_node("GET", "/x", "com.acme.Impl#x/0"));
    let mut edges = vec![handles_route_edge("com.acme.Impl#x/0", "GET", "/x")];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    // Both score 1 ("app"); files have equal length → lexicographic first.
    assert_eq!(prop(route, "path"), Some("/rest/a/v1/x"));
}

#[test]
fn servlet_prefix_config_override_wins() {
    let dir = temp_dir("cfg");
    let out = resolve_servlet_prefix(&dir, &[], Some("/rest"));
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out, Some(("rest".to_string(), "config")));
}

#[test]
fn servlet_prefix_from_web_xml() {
    let dir = temp_dir("web");
    let web = r#"<web-app>
        <servlet>
            <servlet-name>cxf</servlet-name>
            <servlet-class>org.apache.cxf.transport.servlet.CXFServlet</servlet-class>
        </servlet>
        <servlet-mapping>
            <servlet-name>cxf</servlet-name>
            <url-pattern>/services/*</url-pattern>
        </servlet-mapping>
    </web-app>"#;
    std::fs::create_dir_all(dir.join("WEB-INF")).unwrap();
    std::fs::write(dir.join("WEB-INF/web.xml"), web).unwrap();
    let out = resolve_servlet_prefix(&dir, &[], None);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out, Some(("services".to_string(), "web_xml")));
}

#[test]
fn stitch_full_prefix_rewrites_route() {
    let dir = temp_dir("stitch");
    // servlet prefix comes from an osgi_servlet node (no filesystem needed).
    let mut nodes = server_and_bean(
        "/v1/services",
        "restServiceEndPointImpl",
        " com.acme.RestServiceEndPointImpl",
    );
    nodes.push(integration_route(
        "/rest/*",
        "osgi_servlet",
        serde_json::json!({ "servlet_pattern": "/rest/*" }),
    ));
    let handler = "com.acme.RestServiceEndPointImpl#onOffVoice/1";
    nodes.push(route_node("POST", "/sound-box/on-off-voice", handler));
    let mut edges = vec![handles_route_edge(
        handler,
        "POST",
        "/sound-box/on-off-voice",
    )];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    let full = "/rest/v1/services/sound-box/on-off-voice";
    assert_eq!(prop(route, "path"), Some(full));
    assert_eq!(route.id.as_str(), &format!("Route:POST {full}"));
    assert_eq!(prop(route, "local_path"), Some("/sound-box/on-off-voice"));
    assert_eq!(
        prop(route, "servlet_prefix_source"),
        Some("osgi_whiteboard")
    );

    let hr = edges
        .iter()
        .find(|e| e.kind == EdgeKind::HandlesRoute)
        .unwrap();
    assert_eq!(hr.dst.as_str(), &format!("Route:POST {full}"));

    let link = edges
        .iter()
        .find(|e| e.kind == EdgeKind::IntegrationLink && e.reason == "cxf-jaxrs-prefix")
        .expect("provenance IntegrationLink expected");
    assert_eq!(link.dst.as_str(), &format!("Route:POST {full}"));
}

#[test]
fn stitch_without_servlet_layer_uses_address_only() {
    let dir = temp_dir("addr");
    let mut nodes = server_and_bean("/v1/services", "impl", "com.acme.RestServiceEndPointImpl");
    let handler = "com.acme.RestServiceEndPointImpl#onOffVoice/1";
    nodes.push(route_node("POST", "/sound-box/on-off-voice", handler));
    let mut edges = vec![handles_route_edge(
        handler,
        "POST",
        "/sound-box/on-off-voice",
    )];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    assert_eq!(
        prop(route, "path"),
        Some("/v1/services/sound-box/on-off-voice")
    );
    assert_eq!(prop(route, "servlet_prefix_source"), Some("none"));
}

#[test]
fn stitch_no_matching_route_is_noop() {
    let dir = temp_dir("nomatch");
    let mut nodes = server_and_bean("/v1/services", "impl", "com.acme.RestServiceEndPointImpl");
    // A route on an unrelated class — must not be rewritten.
    nodes.push(route_node(
        "GET",
        "/other",
        "com.acme.OtherController#get/0",
    ));
    let mut edges = vec![handles_route_edge(
        "com.acme.OtherController#get/0",
        "GET",
        "/other",
    )];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, Some("/rest"));
    std::fs::remove_dir_all(&dir).ok();

    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    assert_eq!(prop(route, "path"), Some("/other"));
    assert!(
        !edges
            .iter()
            .any(|e| e.kind == EdgeKind::IntegrationLink && e.reason == "cxf-jaxrs-prefix"),
        "no provenance edge should be emitted when nothing matched"
    );
}

fn class_node(fqcn: &str) -> Node {
    Node {
        id: NodeId::new(format!("Class:{fqcn}")),
        kind: NodeKind::Class,
        name: fqcn.rsplit('.').next().unwrap_or(fqcn).to_string(),
        qualified_name: Some(fqcn.to_string()),
        file: "com/acme/X.java".to_string(),
        range: Range::default(),
        props: None,
    }
}

#[test]
fn simple_name_class_resolves_to_unique_fqcn() {
    let dir = temp_dir("simple");
    // bean `class` is a bare simple name, resolved via the unique Class node in the graph.
    let mut nodes = server_and_bean("/crm", "customerSvc", "CustomerService");
    nodes.push(class_node("com.acme.CustomerService"));
    let handler = "com.acme.CustomerService#getCustomer/1";
    nodes.push(route_node("GET", "/customers/{id}", handler));
    let mut edges = vec![handles_route_edge(handler, "GET", "/customers/{id}")];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    assert_eq!(prop(route, "path"), Some("/crm/customers/{id}"));
}

#[test]
fn ambiguous_simple_name_is_not_resolved() {
    let dir = temp_dir("ambig");
    let mut nodes = server_and_bean("/crm", "customerSvc", "CustomerService");
    // Two classes share the simple name → ambiguous → left unresolved → no match.
    nodes.push(class_node("com.acme.CustomerService"));
    nodes.push(class_node("com.other.CustomerService"));
    let handler = "com.acme.CustomerService#getCustomer/1";
    nodes.push(route_node("GET", "/customers/{id}", handler));
    let mut edges = vec![handles_route_edge(handler, "GET", "/customers/{id}")];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    assert_eq!(
        prop(route, "path"),
        Some("/customers/{id}"),
        "ambiguous name must not stitch"
    );
    assert!(!edges.iter().any(|e| e.reason == "cxf-jaxrs-prefix"));
}

#[test]
fn emits_bean_to_class_edge() {
    let dir = temp_dir("beanedge");
    let mut nodes = server_and_bean("/crm", "customerSvc", "com.acme.CustomerService");
    nodes.push(class_node("com.acme.CustomerService"));
    let handler = "com.acme.CustomerService#getCustomer/1";
    nodes.push(route_node("GET", "/customers/{id}", handler));
    let mut edges = vec![handles_route_edge(handler, "GET", "/customers/{id}")];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let edge = edges
        .iter()
        .find(|e| e.reason == "cxf-bean-class")
        .expect("bean → Class registration edge expected");
    assert_eq!(edge.kind, EdgeKind::IntegrationLink);
    assert_eq!(edge.src.as_str(), "IntegrationRoute:spring_xml:customerSvc");
    assert_eq!(edge.dst.as_str(), "Class:com.acme.CustomerService");
}

#[test]
fn no_class_node_means_no_bean_class_edge() {
    let dir = temp_dir("noclass");
    // FQCN bean class, but the class isn't a graph node (e.g. not indexed).
    let mut nodes = server_and_bean("/crm", "customerSvc", "com.acme.CustomerService");
    let handler = "com.acme.CustomerService#getCustomer/1";
    nodes.push(route_node("GET", "/customers/{id}", handler));
    let mut edges = vec![handles_route_edge(handler, "GET", "/customers/{id}")];

    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    // Route is still stitched via the FQCN handler prefix-match …
    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    assert_eq!(prop(route, "path"), Some("/crm/customers/{id}"));
    // … but no bean → Class edge, since the class node doesn't exist.
    assert!(!edges.iter().any(|e| e.reason == "cxf-bean-class"));
}

// ── normalize_prefix / join_url unit tests ───────────────────────────────

#[test]
fn normalize_prefix_variants() {
    assert_eq!(normalize_prefix("/rest/*"), "rest");
    assert_eq!(normalize_prefix("/rest/"), "rest");
    assert_eq!(normalize_prefix("rest"), "rest");
    assert_eq!(normalize_prefix("/api/v1/*"), "api/v1");
    assert_eq!(normalize_prefix("*"), "");
    assert_eq!(normalize_prefix("/"), "");
    assert_eq!(normalize_prefix("  /rest/*  "), "rest");
}

#[test]
fn join_url_variants() {
    assert_eq!(
        join_url(&["rest", "/v1/services", "/a/b"]),
        "/rest/v1/services/a/b"
    );
    assert_eq!(join_url(&["", "/crm", "/x"]), "/crm/x"); // empty servlet prefix collapses
    assert_eq!(join_url(&["/a/", "/b/", "c"]), "/a/b/c"); // dup/trailing slashes normalized
    assert_eq!(join_url(&["", "", ""]), "/");
}

// ── servlet-prefix detectors ─────────────────────────────────────────────

#[test]
fn servlet_prefix_priority_config_over_whiteboard() {
    let dir = temp_dir("prio");
    let nodes = vec![integration_route(
        "/rest/*",
        "osgi_servlet",
        serde_json::json!({ "servlet_pattern": "/rest/*" }),
    )];
    // config override wins over an osgi_servlet node.
    let out = resolve_servlet_prefix(&dir, &nodes, Some("/gateway"));
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out, Some(("gateway".to_string(), "config")));
}

#[test]
fn servlet_prefix_whiteboard_when_no_config() {
    let dir = temp_dir("wb");
    let nodes = vec![integration_route(
        "/rest/*",
        "osgi_servlet",
        serde_json::json!({ "servlet_pattern": "/rest/*" }),
    )];
    let out = resolve_servlet_prefix(&dir, &nodes, None);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out, Some(("rest".to_string(), "osgi_whiteboard")));
}

#[test]
fn servlet_prefix_none_when_nothing_declares_one() {
    let dir = temp_dir("nowt");
    let out = resolve_servlet_prefix(&dir, &[], None);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out, None);
}

#[test]
fn web_xml_picks_cxf_servlet_among_many() {
    let dir = temp_dir("multi-servlet");
    let web = r#"<web-app>
        <servlet>
            <servlet-name>dispatcher</servlet-name>
            <servlet-class>org.springframework.web.servlet.DispatcherServlet</servlet-class>
        </servlet>
        <servlet-mapping><servlet-name>dispatcher</servlet-name><url-pattern>/</url-pattern></servlet-mapping>
        <servlet>
            <servlet-name>cxf</servlet-name>
            <servlet-class>org.apache.cxf.transport.servlet.CXFServlet</servlet-class>
        </servlet>
        <servlet-mapping><servlet-name>cxf</servlet-name><url-pattern>/services/*</url-pattern></servlet-mapping>
    </web-app>"#;
    std::fs::create_dir_all(dir.join("WEB-INF")).unwrap();
    std::fs::write(dir.join("WEB-INF/web.xml"), web).unwrap();
    let out = resolve_servlet_prefix(&dir, &[], None);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out, Some(("services".to_string(), "web_xml")));
}

#[test]
fn web_xml_servlet_name_mismatch_yields_none() {
    let dir = temp_dir("mismatch");
    // CXFServlet present but its mapping uses a different servlet-name.
    let web = r#"<web-app>
        <servlet>
            <servlet-name>cxf</servlet-name>
            <servlet-class>org.apache.cxf.transport.servlet.CXFServlet</servlet-class>
        </servlet>
        <servlet-mapping><servlet-name>other</servlet-name><url-pattern>/nope/*</url-pattern></servlet-mapping>
    </web-app>"#;
    std::fs::create_dir_all(dir.join("WEB-INF")).unwrap();
    std::fs::write(dir.join("WEB-INF/web.xml"), web).unwrap();
    let out = resolve_servlet_prefix(&dir, &[], None);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out, None);
}

#[test]
fn spring_boot_properties_cxf_path_forms() {
    for (body, expect) in [
        ("cxf.path=/api", "api"),
        ("cxf.path = /api", "api"),
        ("cxf.path=\"/api\"", "api"),
        ("# cxf.path=/ignored\ncxf.path=/real", "real"),
    ] {
        let dir = temp_dir("props");
        std::fs::write(dir.join("application.properties"), body).unwrap();
        let out = resolve_servlet_prefix(&dir, &[], None);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            out,
            Some((expect.to_string(), "spring_boot")),
            "body={body:?}"
        );
    }
}

#[test]
fn spring_boot_yaml_nested_and_flat() {
    let dir = temp_dir("yaml-nested");
    std::fs::write(dir.join("application.yml"), "cxf:\n  path: /api\n").unwrap();
    let out = resolve_servlet_prefix(&dir, &[], None);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out, Some(("api".to_string(), "spring_boot")));

    let dir = temp_dir("yaml-flat");
    std::fs::write(dir.join("application.yml"), "cxf.path: \"/gw\"\n").unwrap();
    let out = resolve_servlet_prefix(&dir, &[], None);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out, Some(("gw".to_string(), "spring_boot")));
}

// ── stitch scenarios ─────────────────────────────────────────────────────

#[test]
fn stitch_rewrites_all_routes_of_a_class() {
    let dir = temp_dir("multiroute");
    let mut nodes = server_and_bean("/crm", "svc", "com.acme.Svc");
    nodes.push(route_node("GET", "/customers/{id}", "com.acme.Svc#get/1"));
    nodes.push(route_node("POST", "/customers", "com.acme.Svc#add/1"));
    let mut edges = vec![
        handles_route_edge("com.acme.Svc#get/1", "GET", "/customers/{id}"),
        handles_route_edge("com.acme.Svc#add/1", "POST", "/customers"),
    ];
    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let paths: std::collections::BTreeSet<_> = nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Route)
        .filter_map(|n| prop(n, "path").map(String::from))
        .collect();
    assert!(paths.contains("/crm/customers/{id}"), "paths={paths:?}");
    assert!(paths.contains("/crm/customers"), "paths={paths:?}");
}

#[test]
fn stitch_multiple_servers_route_to_their_own_class() {
    let dir = temp_dir("multiserver");
    let mut nodes = server_and_bean("/crm", "crmSvc", "com.acme.Crm");
    nodes.extend(server_and_bean("/billing", "billSvc", "com.acme.Billing"));
    nodes.push(route_node("GET", "/a", "com.acme.Crm#a/0"));
    nodes.push(route_node("GET", "/b", "com.acme.Billing#b/0"));
    let mut edges = vec![
        handles_route_edge("com.acme.Crm#a/0", "GET", "/a"),
        handles_route_edge("com.acme.Billing#b/0", "GET", "/b"),
    ];
    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let by_handler = |h: &str| {
        nodes
            .iter()
            .find(|n| n.kind == NodeKind::Route && prop(n, "handler") == Some(h))
            .and_then(|n| prop(n, "path").map(String::from))
    };
    assert_eq!(by_handler("com.acme.Crm#a/0").as_deref(), Some("/crm/a"));
    assert_eq!(
        by_handler("com.acme.Billing#b/0").as_deref(),
        Some("/billing/b")
    );
}

#[test]
fn stitch_preserves_existing_class_level_prefix() {
    // Route.path already carries a class-level @Path ("/customerservice"); stitch prepends only.
    let dir = temp_dir("classprefix");
    let mut nodes = server_and_bean("/crm", "svc", "com.acme.Svc");
    nodes.push(route_node(
        "GET",
        "/customerservice/customers/{id}",
        "com.acme.Svc#get/1",
    ));
    let mut edges = vec![handles_route_edge(
        "com.acme.Svc#get/1",
        "GET",
        "/customerservice/customers/{id}",
    )];
    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();
    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    assert_eq!(
        prop(route, "path"),
        Some("/crm/customerservice/customers/{id}")
    );
}

#[test]
fn stitch_blueprint_source_bean_resolves() {
    let dir = temp_dir("bp");
    // Blueprint bean node (source blueprint_xml) + component-id-style ref via the same id.
    let mut nodes = vec![
        integration_route(
            "/api",
            "cxf_jaxrs_server",
            serde_json::json!({ "address": "/api", "beans": ["svc"] }),
        ),
        integration_route(
            "svc",
            "blueprint_xml",
            serde_json::json!({ "class": "com.acme.Bp" }),
        ),
    ];
    nodes.push(route_node("GET", "/x", "com.acme.Bp#x/0"));
    let mut edges = vec![handles_route_edge("com.acme.Bp#x/0", "GET", "/x")];
    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();
    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    assert_eq!(prop(route, "path"), Some("/api/x"));
}

#[test]
fn stitch_inline_service_bean() {
    // Anonymous inline serviceBean: class travels on the server via `bean_classes` (no ref/id).
    let dir = temp_dir("inline");
    let mut nodes = vec![integration_route(
        "/api",
        "cxf_jaxrs_server",
        serde_json::json!({ "address": "/api", "beans": [], "bean_classes": ["com.acme.Inline"] }),
    )];
    nodes.push(class_node("com.acme.Inline"));
    nodes.push(route_node("GET", "/x", "com.acme.Inline#x/0"));
    let mut edges = vec![handles_route_edge("com.acme.Inline#x/0", "GET", "/x")];
    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();

    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    assert_eq!(prop(route, "path"), Some("/api/x"));
    // Inline bean has no bean node — the registration edge originates at the server node.
    let edge = edges
        .iter()
        .find(|e| e.reason == "cxf-bean-class")
        .expect("server → Class edge for inline bean");
    assert_eq!(edge.src.as_str(), "IntegrationRoute:cxf_jaxrs_server:/api");
    assert_eq!(edge.dst.as_str(), "Class:com.acme.Inline");
}

#[test]
fn stitch_inline_bean_simple_name_resolves() {
    let dir = temp_dir("inline-simple");
    let mut nodes = vec![integration_route(
        "/api",
        "cxf_jaxrs_server",
        serde_json::json!({ "address": "/api", "beans": [], "bean_classes": ["Inline"] }),
    )];
    nodes.push(class_node("com.acme.Inline"));
    nodes.push(route_node("GET", "/x", "com.acme.Inline#x/0"));
    let mut edges = vec![handles_route_edge("com.acme.Inline#x/0", "GET", "/x")];
    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();
    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    assert_eq!(
        prop(route, "path"),
        Some("/api/x"),
        "simple inline class should resolve"
    );
}

#[test]
fn bean_class_edge_deduped_when_bean_shared_by_two_servers() {
    let dir = temp_dir("dedup");
    let mut nodes = vec![
        integration_route(
            "/a",
            "cxf_jaxrs_server",
            serde_json::json!({ "address": "/a", "beans": ["svc"] }),
        ),
        integration_route(
            "/b",
            "cxf_jaxrs_server",
            serde_json::json!({ "address": "/b", "beans": ["svc"] }),
        ),
        integration_route(
            "svc",
            "spring_xml",
            serde_json::json!({ "class": "com.acme.Svc" }),
        ),
        class_node("com.acme.Svc"),
    ];
    nodes.push(route_node("GET", "/x", "com.acme.Svc#x/0"));
    let mut edges = vec![handles_route_edge("com.acme.Svc#x/0", "GET", "/x")];
    stitch_route_prefixes(&dir, &mut nodes, &mut edges, None);
    std::fs::remove_dir_all(&dir).ok();
    let bean_class_edges = edges
        .iter()
        .filter(|e| e.reason == "cxf-bean-class")
        .count();
    assert_eq!(
        bean_class_edges, 1,
        "bean → Class edge must be deduped across servers"
    );
}
