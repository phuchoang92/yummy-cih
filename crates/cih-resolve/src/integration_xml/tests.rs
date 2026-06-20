use super::*;

const CAMEL: &str = r#"
    <beans xmlns="http://camel.apache.org/schema/spring">
      <camelContext id="ctx">
        <route id="r1">
          <from uri="jms:queue:incoming-orders"/>
          <to uri="direct:processOrder"/>
          <to uri="activemq:topic:order-events"/>
        </route>
      </camelContext>
    </beans>
"#;

#[test]
fn camel_extracts_routes_and_destinations() {
    let out = extract_integration_xml("camel-routes.xml", CAMEL);
    assert!(out
        .nodes
        .iter()
        .any(|n| n.kind == NodeKind::IntegrationRoute));
    assert!(out
        .nodes
        .iter()
        .any(|n| n.kind == NodeKind::MessageDestination && n.name == "order-events"));
    assert!(out.edges.iter().any(|e| e.kind == EdgeKind::PublishesEvent));
    assert!(out
        .edges
        .iter()
        .any(|e| e.kind == EdgeKind::IntegrationLink));
}

#[test]
fn parse_camel_uri_strips_jms_queue() {
    assert_eq!(parse_camel_uri("jms:queue:my-queue"), ("jms", "my-queue"));
    assert_eq!(parse_camel_uri("direct:my-route"), ("direct", "my-route"));
    assert_eq!(
        parse_camel_uri("kafka:topic-a?brokers=x"),
        ("kafka", "topic-a")
    );
}

#[test]
fn non_integration_xml_yields_nothing() {
    let out = extract_integration_xml("pom.xml", "<project><modelVersion/></project>");
    assert!(out.nodes.is_empty());
    assert!(out.edges.is_empty());
}

#[test]
fn spring_beans_extracts_beans() {
    let xml = r#"<beans xmlns="http://www.springframework.org/schema/beans">
        <bean id="orderService" class="com.acme.OrderService"/>
    </beans>"#;
    let out = extract_integration_xml("beans.xml", xml);
    assert_eq!(out.nodes.len(), 1);
    assert_eq!(out.nodes[0].kind, NodeKind::IntegrationRoute);
    assert_eq!(out.nodes[0].name, "orderService");
}
