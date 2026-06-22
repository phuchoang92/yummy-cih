use std::collections::HashMap;

use crate::entry::FeatureGroupEntry;
use crate::strategy::{FeatureStrategy, StrategyInput};

/// Sequential strategy composer. Runs inner strategies in order; each receives the
/// accumulated prior assignments so context-aware strategies (structural, embed)
/// can use earlier results.
///
/// Merge rule: for each node, the last assignment from a non-catch-all strategy wins.
/// A later strategy's catch-all result does NOT override an earlier non-catch-all result.
pub struct HybridStrategy {
    strategies: Vec<Box<dyn FeatureStrategy>>,
    catch_all_features: Vec<String>,
}

impl HybridStrategy {
    pub fn new(strategies: Vec<Box<dyn FeatureStrategy>>, catch_all_features: Vec<String>) -> Self {
        Self {
            strategies,
            catch_all_features,
        }
    }

    fn is_catch_all(&self, feature: &str) -> bool {
        self.catch_all_features
            .iter()
            .any(|c| c.as_str() == feature)
    }
}

impl FeatureStrategy for HybridStrategy {
    fn name(&self) -> &str {
        "hybrid"
    }

    fn feature_of(&self, file: &str) -> String {
        for s in &self.strategies {
            let f = s.feature_of(file);
            if !f.is_empty() && f != "shared" {
                return f;
            }
        }
        "shared".to_string()
    }

    fn assign(&self, input: &StrategyInput<'_>) -> Vec<FeatureGroupEntry> {
        // node_id → best entry so far
        let mut assignments: HashMap<String, FeatureGroupEntry> = HashMap::new();

        for strategy in &self.strategies {
            // Build prior slice from current state
            let prior: Vec<FeatureGroupEntry> = assignments.values().cloned().collect();

            let sub_input = StrategyInput {
                nodes: input.nodes,
                edges: input.edges,
                graph_version: input.graph_version,
                prior_assignments: &prior,
            };

            for entry in strategy.assign(&sub_input) {
                let new_is_catch_all = self.is_catch_all(&entry.name);
                match assignments.get(&entry.node_id) {
                    None => {
                        assignments.insert(entry.node_id.clone(), entry);
                    }
                    Some(existing) => {
                        let existing_is_catch_all = self.is_catch_all(&existing.name);
                        // Override when: new is non-catch-all, OR existing is catch-all and new
                        // has higher confidence.
                        if !new_is_catch_all
                            || (existing_is_catch_all
                                && entry.confidence > existing.confidence)
                        {
                            assignments.insert(entry.node_id.clone(), entry);
                        }
                    }
                }
            }
        }

        assignments.into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use cih_core::{Node, NodeId, NodeKind, Range};

    use super::*;
    use crate::config::PackageConfig;
    use crate::strategies::package::PackageStrategy;
    use crate::strategies::structural::{StructuralConfig, StructuralStrategy};

    fn make_node(id: &str, name: &str, file: &str) -> Node {
        Node {
            id: NodeId::new(id.to_string()),
            kind: NodeKind::Class,
            name: name.to_string(),
            qualified_name: None,
            file: file.to_string(),
            range: Range::default(),
            props: None,
        }
    }

    fn default_catch_all() -> Vec<String> {
        vec!["shared".into(), "core".into(), "common".into()]
    }

    #[test]
    fn package_non_catchall_overrides_structural_shared() {
        // Structural sees "PaymentFilter" (name keyword) but not a path fragment → only 1 signal
        // → structural does NOT assign this (min_signals=2).
        // Package sees payment-service/... → assigns "payment".
        // Final: "payment"
        let mut s_cfg = StructuralConfig::default();
        s_cfg.min_signals = 2;
        let structural = Box::new(StructuralStrategy::new(s_cfg));
        let package = Box::new(PackageStrategy::new(PackageConfig::default()));
        let hybrid = HybridStrategy::new(
            vec![structural, package],
            default_catch_all(),
        );
        let node = make_node(
            "Class:com.example.payment.PaymentFilter",
            "PaymentFilter",
            "payment-service/src/main/java/com/example/payment/PaymentFilter.java",
        );
        let input = StrategyInput {
            nodes: &[node],
            edges: &[],
            graph_version: "v1",
            prior_assignments: &[],
        };
        let entries = hybrid.assign(&input);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "payment");
    }

    #[test]
    fn catch_all_does_not_override_domain_assignment() {
        // Package gives "payment" first. Structural gives "shared" (path=common).
        // Structural runs second here — but its catch-all must NOT override package's "payment".
        let package = Box::new(PackageStrategy::new(PackageConfig::default()));
        let mut s_cfg = StructuralConfig::default();
        s_cfg.min_signals = 1; // lower threshold so structural fires on path
        let structural = Box::new(StructuralStrategy::new(s_cfg));
        let hybrid = HybridStrategy::new(
            vec![package, structural], // package first, structural second
            default_catch_all(),
        );
        let node = make_node(
            "Class:com.example.PaymentService",
            "PaymentService",
            "payment-service/src/main/java/com/example/payment/PaymentService.java",
        );
        let input = StrategyInput {
            nodes: &[node],
            edges: &[],
            graph_version: "v1",
            prior_assignments: &[],
        };
        let entries = hybrid.assign(&input);
        assert_eq!(entries.len(), 1);
        // Package assigned "payment"; structural would need min_signals=1 AND name OR path signal.
        // "PaymentService" does NOT match structural name keywords → structural won't fire.
        assert_eq!(entries[0].name, "payment", "package domain should not be overridden");
    }

    #[test]
    fn feature_of_delegates_in_order() {
        let package = Box::new(PackageStrategy::new(PackageConfig::default()));
        let hybrid = HybridStrategy::new(vec![package], default_catch_all());
        assert_eq!(
            hybrid.feature_of("payment-service/src/main/java/com/example/PaymentService.java"),
            "payment"
        );
        assert_eq!(hybrid.feature_of("unknown.java"), "shared");
    }
}
