mod cache;
mod class_enrich;
mod community_enrich;
mod config;
mod feature_enrich;
mod flow_enrich;
mod loader;
mod run;

pub use crate::llm::{LlmCallConfig, LlmProvider};
pub use class_enrich::enrich_classes_for_chains;
pub use config::{WikiConfig, WikiGrouping, WikiMode};
pub use feature_enrich::{
    build_feature_evidence, build_feature_user_prompt, cached_feature_summary, enrich_one_feature,
    parse_feature_summary, retain_matching_feature_groups,
};
pub use flow_enrich::parse_flow_summary;
pub use loader::community_matches_route_prefix;
pub use run::run_wiki;
pub(crate) use run::wiki_needs_regen;

#[cfg(test)]
mod tests;
