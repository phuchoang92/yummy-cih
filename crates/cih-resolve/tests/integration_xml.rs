use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};
use cih_resolve::integration_xml::{extract_integration_xml, parse_camel_uri};
use cih_resolve::{resolve_cxf_servlet_prefix, resolve_jaxrs_xml_prefixes};

#[test]
fn parse_camel_uri_jms_queue() {
    let (scheme, name) = parse_camel_uri("jms:queue:my-orders");
    assert_eq!(scheme, "jms");
    assert_eq!(name, "my-orders");
}

#[test]
fn parse_camel_uri_direct() {
    let (scheme, name) = parse_camel_uri("direct:process-payment");
    assert_eq!(scheme, "direct");
    assert_eq!(name, "process-payment");
}

#[test]
fn parse_camel_uri_strips_query_params() {
    let (scheme, name) = parse_camel_uri("kafka:my-topic?brokers=localhost:9092");
    assert_eq!(scheme, "kafka");
    assert_eq!(name, "my-topic");
}

#[test]
fn camel_xml_emits_route_node_from_from_uri() {
    let xml = r#"<camelContext xmlns="http://camel.apache.org/schema/spring">
        <route id="order-route">
            <from uri="jms:queue:orders"/>
            <to uri="direct:process"/>
        </route>
    </camelContext>"#;
    let out = extract_integration_xml("src/main/resources/camel-routes.xml", xml);
    assert!(!out.nodes.is_empty(), "should emit at least one route node");
    let route_node = out.nodes.iter().find(|n| n.kind == NodeKind::IntegrationRoute);
    assert!(route_node.is_some(), "IntegrationRoute node expected");
    let route = route_node.unwrap();
    assert!(
        route.name.contains("jms"),
        "route node name should include scheme: {}",
        route.name
    );
}

#[test]
fn camel_xml_emits_message_destination_for_broker_to() {
    let xml = r#"<camelContext xmlns="http://camel.apache.org/schema/spring">
        <route>
            <from uri="direct:trigger"/>
            <to uri="kafka:payment-events"/>
        </route>
    </camelContext>"#;
    let out = extract_integration_xml("camel.xml", xml);
    let dest = out.nodes.iter().find(|n| n.kind == NodeKind::MessageDestination);
    assert!(dest.is_some(), "MessageDestination node expected for kafka: to");
    let edge = out.edges.iter().find(|e| e.kind == EdgeKind::PublishesEvent);
    assert!(edge.is_some(), "PublishesEvent edge expected");
}

#[test]
fn non_integration_xml_returns_empty() {
    let xml = r#"<?xml version="1.0"?><project><version>1.0</version></project>"#;
    let out = extract_integration_xml("pom.xml", xml);
    assert!(out.nodes.is_empty());
    assert!(out.edges.is_empty());
}

#[test]
fn blueprint_xml_emits_integration_route_for_service() {
    let xml = r#"<blueprint xmlns="http://www.osgi.org/xmlns/blueprint">
        <bean id="orderSvc" class="com.acme.OrderServiceImpl"/>
        <service ref="orderSvc" interface="com.acme.OrderService"/>
    </blueprint>"#;
    let out = extract_integration_xml("OSGI-INF/blueprint/wiring.xml", xml);
    let route = out.nodes.iter().find(|n| n.kind == NodeKind::IntegrationRoute);
    assert!(route.is_some(), "IntegrationRoute node expected for blueprint service");
}

// ── CXF JAX-RS base-path extraction + stitching ─────────────────────────────

fn prop<'a>(node: &'a Node, key: &str) -> Option<&'a str> {
    node.props.as_ref()?.get(key)?.as_str()
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

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("cih-jaxrs-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn cxf_jaxrs_server_spring_extracts_address_and_bean() {
    // Mirrors servicemix cxf-jaxrs beans.xml (note the leading space in `class`).
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans"
        xmlns:jaxrs="http://cxf.apache.org/jaxrs">
        <jaxrs:server id="customerService" address="/crm">
            <jaxrs:serviceBeans>
                <ref bean="customerSvc"/>
            </jaxrs:serviceBeans>
        </jaxrs:server>
        <bean id="customerSvc" class=" org.apache.servicemix.examples.cxf.jaxrs.CustomerService"/>
    </beans>"#;
    let out = extract_integration_xml("META-INF/spring/beans.xml", xml);
    let server = out
        .nodes
        .iter()
        .find(|n| prop(n, "source") == Some("cxf_jaxrs_server"))
        .expect("cxf_jaxrs_server node expected");
    assert_eq!(server.name, "/crm");
    assert_eq!(prop(server, "address"), Some("/crm"));
    assert_eq!(prop(server, "bean_id"), Some("customerSvc"));
}

#[test]
fn cxf_jaxrs_server_blueprint_component_id_ref() {
    let xml = r#"<blueprint xmlns="http://www.osgi.org/xmlns/blueprint"
        xmlns:jaxrs="http://cxf.apache.org/blueprint/jaxrs">
        <jaxrs:server id="svc" address="/api">
            <jaxrs:serviceBeans>
                <ref component-id="customerSvc"/>
            </jaxrs:serviceBeans>
        </jaxrs:server>
        <bean id="customerSvc" class="com.acme.CustomerService"/>
    </blueprint>"#;
    let out = extract_integration_xml("OSGI-INF/blueprint/blueprint.xml", xml);
    let server = out
        .nodes
        .iter()
        .find(|n| prop(n, "source") == Some("cxf_jaxrs_server"))
        .expect("cxf_jaxrs_server node expected in blueprint");
    assert_eq!(prop(server, "address"), Some("/api"));
    assert_eq!(prop(server, "bean_id"), Some("customerSvc"));
}

#[test]
fn cxf_jaxrs_server_without_address_is_skipped() {
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans"
        xmlns:jaxrs="http://cxf.apache.org/jaxrs">
        <jaxrs:server id="noAddr">
            <jaxrs:serviceBeans><ref bean="svc"/></jaxrs:serviceBeans>
        </jaxrs:server>
        <bean id="svc" class="com.acme.Svc"/>
    </beans>"#;
    let out = extract_integration_xml("META-INF/spring/beans.xml", xml);
    assert!(
        out.nodes
            .iter()
            .all(|n| prop(n, "source") != Some("cxf_jaxrs_server")),
        "server with no address should be skipped"
    );
}

#[test]
fn osgi_whiteboard_servlet_pattern_node() {
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans">
        <bean id="cxfServlet" class="org.apache.cxf.transport.servlet.CXFServlet"/>
        <osgi:service ref="cxfServlet" interface="javax.servlet.Servlet"
            xmlns:osgi="http://www.springframework.org/schema/osgi">
            <osgi:service-properties>
                <entry key="osgi.http.whiteboard.servlet.pattern" value="/rest/*"/>
            </osgi:service-properties>
        </osgi:service>
    </beans>"#;
    let out = extract_integration_xml("META-INF/spring/beans_rest_web_servlets.xml", xml);
    let servlet = out
        .nodes
        .iter()
        .find(|n| prop(n, "source") == Some("osgi_servlet"));
    assert!(servlet.is_some(), "osgi_servlet node expected");
    assert_eq!(prop(servlet.unwrap(), "servlet_pattern"), Some("/rest/*"));
}

#[test]
fn servlet_prefix_config_override_wins() {
    let dir = temp_dir("cfg");
    let out = resolve_cxf_servlet_prefix(&dir, &[], Some("/rest"));
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
    let out = resolve_cxf_servlet_prefix(&dir, &[], None);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out, Some(("services".to_string(), "web_xml")));
}

#[test]
fn stitch_full_prefix_rewrites_route() {
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans"
        xmlns:jaxrs="http://cxf.apache.org/jaxrs">
        <jaxrs:server id="restService" address="/v1/services">
            <jaxrs:serviceBeans><ref bean="restServiceEndPointImpl"/></jaxrs:serviceBeans>
        </jaxrs:server>
        <bean id="restServiceEndPointImpl" class=" com.acme.RestServiceEndPointImpl"/>
    </beans>"#;
    let mut nodes = extract_integration_xml("META-INF/spring/beans_rest.xml", xml).nodes;
    let handler = "com.acme.RestServiceEndPointImpl#onOffVoice/1";
    nodes.push(route_node("POST", "/sound-box/on-off-voice", handler));
    let mut edges = vec![handles_route_edge(handler, "POST", "/sound-box/on-off-voice")];

    resolve_jaxrs_xml_prefixes(&mut nodes, &mut edges, Some(("/rest", "osgi_whiteboard")));

    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    let full = "/rest/v1/services/sound-box/on-off-voice";
    assert_eq!(prop(route, "path"), Some(full));
    assert_eq!(route.id.as_str(), &format!("Route:POST {full}"));
    assert_eq!(prop(route, "local_path"), Some("/sound-box/on-off-voice"));
    assert_eq!(prop(route, "servlet_prefix_source"), Some("osgi_whiteboard"));

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
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans"
        xmlns:jaxrs="http://cxf.apache.org/jaxrs">
        <jaxrs:server id="s" address="/v1/services">
            <jaxrs:serviceBeans><ref bean="impl"/></jaxrs:serviceBeans>
        </jaxrs:server>
        <bean id="impl" class="com.acme.RestServiceEndPointImpl"/>
    </beans>"#;
    let mut nodes = extract_integration_xml("META-INF/spring/beans_rest.xml", xml).nodes;
    let handler = "com.acme.RestServiceEndPointImpl#onOffVoice/1";
    nodes.push(route_node("POST", "/sound-box/on-off-voice", handler));
    let mut edges = vec![handles_route_edge(handler, "POST", "/sound-box/on-off-voice")];

    resolve_jaxrs_xml_prefixes(&mut nodes, &mut edges, None);

    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    assert_eq!(prop(route, "path"), Some("/v1/services/sound-box/on-off-voice"));
    assert_eq!(prop(route, "servlet_prefix_source"), Some("none"));
}

#[test]
fn stitch_no_matching_route_is_noop() {
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans"
        xmlns:jaxrs="http://cxf.apache.org/jaxrs">
        <jaxrs:server id="s" address="/v1/services">
            <jaxrs:serviceBeans><ref bean="impl"/></jaxrs:serviceBeans>
        </jaxrs:server>
        <bean id="impl" class="com.acme.RestServiceEndPointImpl"/>
    </beans>"#;
    let mut nodes = extract_integration_xml("beans_rest.xml", xml).nodes;
    // A route on an unrelated class — must not be rewritten.
    nodes.push(route_node("GET", "/other", "com.acme.OtherController#get/0"));
    let mut edges = vec![handles_route_edge("com.acme.OtherController#get/0", "GET", "/other")];

    resolve_jaxrs_xml_prefixes(&mut nodes, &mut edges, Some(("/rest", "osgi_whiteboard")));

    let route = nodes.iter().find(|n| n.kind == NodeKind::Route).unwrap();
    assert_eq!(prop(route, "path"), Some("/other"));
    assert!(
        !edges
            .iter()
            .any(|e| e.kind == EdgeKind::IntegrationLink && e.reason == "cxf-jaxrs-prefix"),
        "no provenance edge should be emitted when nothing matched"
    );
}
