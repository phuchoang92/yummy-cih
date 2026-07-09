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
                            || (existing_is_catch_all && entry.confidence > existing.confidence)
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
