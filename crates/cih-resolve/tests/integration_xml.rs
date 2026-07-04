use cih_core::{EdgeKind, NodeKind};
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
