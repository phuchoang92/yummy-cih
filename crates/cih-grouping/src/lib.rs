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
