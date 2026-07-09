use cih_core::{EdgeKind, Node, NodeKind};
use cih_resolve::integration_xml::{extract_integration_xml, parse_camel_uri};

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

// ── More CXF JAX-RS XML shapes ───────────────────────────────────────────────

fn servers(out: &cih_resolve::integration_xml::IntegrationXmlOutput) -> Vec<&Node> {
    out.nodes
        .iter()
        .filter(|n| prop(n, "source") == Some("cxf_jaxrs_server"))
        .collect()
}

fn beans_of(n: &Node) -> Vec<&str> {
    n.props
        .as_ref()
        .and_then(|p| p.get("beans"))
        .and_then(|b| b.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default()
}

#[test]
fn multiple_jaxrs_servers_in_one_file() {
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans"
        xmlns:jaxrs="http://cxf.apache.org/jaxrs">
        <jaxrs:server id="a" address="/crm">
            <jaxrs:serviceBeans><ref bean="crmSvc"/></jaxrs:serviceBeans>
        </jaxrs:server>
        <jaxrs:server id="b" address="/billing">
            <jaxrs:serviceBeans><ref bean="billingSvc"/></jaxrs:serviceBeans>
        </jaxrs:server>
        <bean id="crmSvc" class="com.acme.Crm"/>
        <bean id="billingSvc" class="com.acme.Billing"/>
    </beans>"#;
    let out = extract_integration_xml("beans.xml", xml);
    let addrs: std::collections::BTreeSet<_> =
        servers(&out).iter().map(|n| n.name.clone()).collect();
    assert!(addrs.contains("/crm") && addrs.contains("/billing"), "addrs={addrs:?}");
    assert_eq!(servers(&out).len(), 2);
}

#[test]
fn jaxrs_server_with_multiple_service_beans() {
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans"
        xmlns:jaxrs="http://cxf.apache.org/jaxrs">
        <jaxrs:server id="s" address="/v1">
            <jaxrs:serviceBeans>
                <ref bean="one"/>
                <ref bean="two"/>
            </jaxrs:serviceBeans>
        </jaxrs:server>
    </beans>"#;
    let out = extract_integration_xml("beans.xml", xml);
    let server = servers(&out).into_iter().next().expect("server");
    let beans = beans_of(server);
    assert!(beans.contains(&"one") && beans.contains(&"two"), "beans={beans:?}");
}

#[test]
fn jaxrs_server_attr_order_and_single_quotes() {
    // address before id, single-quoted attribute values.
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans"
        xmlns:jaxrs="http://cxf.apache.org/jaxrs">
        <jaxrs:server address='/crm' id='s'>
            <jaxrs:serviceBeans><ref bean='svc'/></jaxrs:serviceBeans>
        </jaxrs:server>
    </beans>"#;
    let out = extract_integration_xml("beans.xml", xml);
    let server = servers(&out).into_iter().next().expect("server");
    assert_eq!(prop(server, "address"), Some("/crm"));
    assert_eq!(beans_of(server), vec!["svc"]);
}

#[test]
fn jaxrs_ref_paired_and_self_closing_forms() {
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans"
        xmlns:jaxrs="http://cxf.apache.org/jaxrs">
        <jaxrs:server id="s" address="/v1">
            <jaxrs:serviceBeans>
                <ref bean="selfClosed"/>
                <ref bean="paired"></ref>
            </jaxrs:serviceBeans>
        </jaxrs:server>
    </beans>"#;
    let out = extract_integration_xml("beans.xml", xml);
    let beans = beans_of(servers(&out).into_iter().next().unwrap());
    assert!(beans.contains(&"selfClosed") && beans.contains(&"paired"), "beans={beans:?}");
}

#[test]
fn cxf_only_file_without_beans_still_parses_server() {
    // Spring beans namespace present but no <bean>; the CXF namespace drives the "cxf" dispatch.
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans"
        xmlns:jaxrs="http://cxf.apache.org/jaxrs"
        xsi:schemaLocation="http://cxf.apache.org/jaxrs http://cxf.apache.org/schemas/jaxrs.xsd">
        <jaxrs:server id="s" address="/api">
            <jaxrs:serviceBeans><ref bean="svc"/></jaxrs:serviceBeans>
        </jaxrs:server>
    </beans>"#;
    let out = extract_integration_xml("beans.xml", xml);
    let server = servers(&out).into_iter().next().expect("server via cxf dispatch");
    assert_eq!(prop(server, "address"), Some("/api"));
}

#[test]
fn osgi_whiteboard_root_pattern() {
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans">
        <entry key="osgi.http.whiteboard.servlet.pattern" value="/*"/>
    </beans>"#;
    let out = extract_integration_xml("beans_web.xml", xml);
    let servlet = out.nodes.iter().find(|n| prop(n, "source") == Some("osgi_servlet"));
    assert_eq!(prop(servlet.expect("servlet node"), "servlet_pattern"), Some("/*"));
}

// ── Edge cases: fixed behaviors + documented limitations ─────────────────────

#[test]
fn blueprint_bean_class_is_captured() {
    // Fixed: blueprint `<bean id class>` is now emitted so a CXF jaxrs:server ref resolves.
    let xml = r#"<blueprint xmlns="http://www.osgi.org/xmlns/blueprint"
        xmlns:jaxrs="http://cxf.apache.org/blueprint/jaxrs">
        <jaxrs:server id="s" address="/api">
            <jaxrs:serviceBeans><ref component-id="svc"/></jaxrs:serviceBeans>
        </jaxrs:server>
        <bean id="svc" class="com.acme.CustomerService"/>
    </blueprint>"#;
    let out = extract_integration_xml("OSGI-INF/blueprint/blueprint.xml", xml);
    let bean = out
        .nodes
        .iter()
        .find(|n| n.name == "svc" && prop(n, "class") == Some("com.acme.CustomerService"))
        .expect("blueprint <bean> class node");
    assert_eq!(prop(bean, "source"), Some("blueprint_xml"));
}

#[test]
fn blueprint_service_and_bean_same_id_both_survive() {
    // A <service ref="svc"> and a <bean id="svc"> must not collide on node id.
    let xml = r#"<blueprint xmlns="http://www.osgi.org/xmlns/blueprint">
        <service ref="svc" interface="com.acme.Api"/>
        <bean id="svc" class="com.acme.ApiImpl"/>
    </blueprint>"#;
    let out = extract_integration_xml("OSGI-INF/blueprint/wiring.xml", xml);
    let ids: std::collections::BTreeSet<_> = out.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids.len(), out.nodes.len(), "node ids must be unique: {ids:?}");
    assert!(out.nodes.iter().any(|n| prop(n, "class") == Some("com.acme.ApiImpl")));
    assert!(out.nodes.iter().any(|n| prop(n, "interface") == Some("com.acme.Api")));
}

#[test]
fn commented_out_server_is_not_parsed() {
    // Fixed: a commented-out <jaxrs:server> must not create a phantom route prefix.
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans"
        xmlns:jaxrs="http://cxf.apache.org/jaxrs">
        <!-- <jaxrs:server id="dead" address="/commented-out"><jaxrs:serviceBeans><ref bean="x"/></jaxrs:serviceBeans></jaxrs:server> -->
        <jaxrs:server id="live" address="/live">
            <jaxrs:serviceBeans><ref bean="x"/></jaxrs:serviceBeans>
        </jaxrs:server>
        <bean id="x" class="com.acme.X"/>
    </beans>"#;
    let out = extract_integration_xml("beans.xml", xml);
    assert!(
        !out.nodes.iter().any(|n| prop(n, "address") == Some("/commented-out")),
        "commented-out server must not be parsed"
    );
    assert!(
        out.nodes.iter().any(|n| prop(n, "address") == Some("/live")),
        "the live server must still be parsed"
    );
}

#[test]
fn commented_out_bean_is_not_parsed() {
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans">
        <!-- <bean id="dead" class="com.acme.Dead"/> -->
        <bean id="live" class="com.acme.Live"/>
    </beans>"#;
    let out = extract_integration_xml("beans.xml", xml);
    assert!(!out.nodes.iter().any(|n| prop(n, "class") == Some("com.acme.Dead")));
    assert!(out.nodes.iter().any(|n| prop(n, "class") == Some("com.acme.Live")));
}

#[test]
fn aliased_jaxrs_namespace_prefix_is_matched() {
    // The namespace-aware parser matches <s:server> by namespace URI, not the literal `jaxrs:`.
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans"
        xmlns:s="http://cxf.apache.org/jaxrs">
        <s:server id="s" address="/api">
            <s:serviceBeans><ref bean="svc"/></s:serviceBeans>
        </s:server>
        <bean id="svc" class="com.acme.Svc"/>
    </beans>"#;
    let out = extract_integration_xml("beans.xml", xml);
    let server = out
        .nodes
        .iter()
        .find(|n| prop(n, "source") == Some("cxf_jaxrs_server"))
        .expect("aliased-prefix server should be matched");
    assert_eq!(prop(server, "address"), Some("/api"));
    assert_eq!(beans_of(server), vec!["svc"]);
}

fn bean_classes_of(n: &Node) -> Vec<&str> {
    n.props
        .as_ref()
        .and_then(|p| p.get("bean_classes"))
        .and_then(|b| b.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default()
}

#[test]
fn inline_service_bean_is_captured() {
    // An anonymous inline <bean class> inside <jaxrs:serviceBeans> is now captured on the server.
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans"
        xmlns:jaxrs="http://cxf.apache.org/jaxrs">
        <jaxrs:server id="s" address="/api">
            <jaxrs:serviceBeans>
                <bean class="com.acme.InlineSvc"/>
            </jaxrs:serviceBeans>
        </jaxrs:server>
    </beans>"#;
    let out = extract_integration_xml("beans.xml", xml);
    let server = out.nodes.iter().find(|n| prop(n, "source") == Some("cxf_jaxrs_server")).unwrap();
    assert_eq!(bean_classes_of(server), vec!["com.acme.InlineSvc"]);
}

#[test]
fn namespaced_beans_prefix_and_entity_are_parsed() {
    // A prefix on the beans namespace (<beans:bean>) and an XML entity in an attribute value.
    let xml = r#"<beans:beans xmlns:beans="http://www.springframework.org/schema/beans">
        <beans:bean id="a&amp;b" class="com.acme.Amp"/>
    </beans:beans>"#;
    let out = extract_integration_xml("beans.xml", xml);
    let bean = out
        .nodes
        .iter()
        .find(|n| prop(n, "class") == Some("com.acme.Amp"))
        .expect("namespaced <beans:bean> should parse");
    assert_eq!(bean.name, "a&b", "entity in attribute must be decoded");
}
