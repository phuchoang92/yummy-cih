//! Cross-repo feature grouping — organizes a multi-repo system into feature
//! groups and matches producer↔consumer contracts across services.
//!
//! Where [`cih-community`](../cih_community/index.html) groups symbols *within* a
//! repo, this crate groups *repos* and their surfaces: it applies a grouping
//! strategy (package, embedding, or embedding-cluster) to assign features, honors
//! user overrides, and writes the group artifacts the cross-repo tools
//! (`group_contracts`, `api_impact`, `trace_flow_x`) read. Operates over
//! `cih-core` group/contract types.

pub mod artifact;
pub mod config;
pub mod entry;
pub mod overrides;
pub mod strategies;
pub mod strategy;

pub use artifact::{
    feature_artifact_dir, find_feature_artifact_dir, prune_feature_artifacts,
    read_feature_artifact, write_feature_artifacts,
};
pub use config::PackageConfig;
pub use entry::{fnv64_node, FeatureGroupEntry};
pub use overrides::{apply_overrides, FeatureOverrideEntry, FeatureOverrides};
pub use strategies::embed::{EmbedConfig, EmbedStrategy};
pub use strategies::embed_cluster::{
    is_clusterable_kind, EmbedClusterConfig, EmbedClusterStrategy, NodeMeta,
};
pub use strategies::hybrid::HybridStrategy;
pub use strategies::llm::{FeatureLlmCaller, LlmConfig, LlmStrategy};
pub use strategies::package::PackageStrategy;
pub use strategies::structural::{StructuralConfig, StructuralStrategy};
pub use strategy::{Embedder, FeatureStrategy, StrategyInput};
