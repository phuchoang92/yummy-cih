use super::*;

fn strategy() -> PackageStrategy {
    PackageStrategy::new(PackageConfig::default())
}

#[test]
fn modules_segment_takes_priority() {
    let s = strategy();
    assert_eq!(
        s.feature_of("src/main/java/com/example/modules/order/service/OrderService.java"),
        "order"
    );
    assert_eq!(
        s.feature_of("src/main/java/org/phuc/commerce/modules/cart/CartController.java"),
        "cart"
    );
}

#[test]
fn maven_module_name_normalisation() {
    let s = strategy();
    assert_eq!(
        s.feature_of("banking-overdraft/src/main/java/com/bank/overdraft/OverdraftService.java"),
        "overdraft"
    );
    assert_eq!(
        s.feature_of(
            "banking-overdraft-api/src/main/java/com/bank/overdraft/api/OverdraftApi.java"
        ),
        "overdraft"
    );
    assert_eq!(
        s.feature_of("payment-service/src/main/java/com/example/PaymentProcessor.java"),
        "payment"
    );
    assert_eq!(
        s.feature_of("payment-service-impl/src/main/java/com/example/PaymentImpl.java"),
        "payment"
    );
}

#[test]
fn catch_all_module_falls_through_to_package() {
    let s = strategy();
    assert_eq!(
        s.feature_of("custom-impl/src/main/java/com/bank/overdraft/CustomOverdraftImpl.java"),
        "overdraft"
    );
    assert_eq!(
        s.feature_of("core/src/main/java/com/example/payment/gateway/PaymentGateway.java"),
        "payment"
    );
}

#[test]
fn no_known_structure_returns_shared() {
    let s = strategy();
    assert_eq!(s.feature_of("SomeClass.java"), "shared");
    assert_eq!(s.feature_of(""), "shared");
}

#[test]
fn custom_config_strip_prefixes() {
    let mut cfg = PackageConfig::default();
    cfg.strip_prefixes.push("myco-".into());
    let s = PackageStrategy::new(cfg);
    assert_eq!(
        s.feature_of("myco-payments/src/main/java/com/myco/PaymentService.java"),
        "payments"
    );
}

#[test]
fn classify_returns_evidence() {
    let s = strategy();
    let (feat, ev) = s.classify("payment-service/src/main/java/com/example/PaymentService.java");
    assert_eq!(feat, "payment");
    assert!(ev.contains("payment-service"), "evidence: {ev}");
}
