use std::sync::Arc;

use anyhow::Result;
use cih_embed::{EmbedModel, EmbedModelKind};
use cih_grouping::{
    EmbedConfig, EmbedStrategy, Embedder, FeatureGroupEntry, FeatureLlmCaller, FeatureStrategy,
    HybridStrategy, LlmConfig, LlmStrategy, PackageConfig, PackageStrategy, StructuralConfig,
    StructuralStrategy,
};

use crate::llm::{LlmAdapter, LlmRequest};

/// Config for the optional LLM feature-classification stage.
pub struct FeatureLlmOptions {
    pub adapter: Box<dyn LlmAdapter>,
    pub api_key: Option<String>,
    pub model: String,
    pub max_tokens: u32,
    pub timeout_secs: u64,
    /// Entries from the previous run's artifact file, used for incremental cache.
    pub prior_artifact: Vec<FeatureGroupEntry>,
}

/// `cih-embed::EmbedModel` wrapped as a `cih-grouping::Embedder`.
struct EngineEmbedder {
    model: EmbedModel,
}

impl Embedder for EngineEmbedder {
    fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.model.embed(texts)
    }
}

/// `cih-engine::LlmAdapter` wrapped as a `cih-grouping::FeatureLlmCaller`.
struct EngineLlmCaller {
    adapter: Box<dyn LlmAdapter>,
    api_key: Option<String>,
    model: String,
    max_tokens: u32,
    timeout_secs: u64,
}

impl FeatureLlmCaller for EngineLlmCaller {
    fn classify_batch(&self, system: &str, user: &str) -> anyhow::Result<String> {
        let req = LlmRequest {
            system: system.to_string(),
            user: user.to_string(),
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            timeout_secs: self.timeout_secs,
        };
        let resp = self.adapter.call(self.api_key.as_deref(), &req)?;
        Ok(resp.text)
    }
}

/// Build the feature classification strategy selected by the `--feature-strategy` flag.
///
/// - `"package"` (default): fast file-path heuristic, zero cost.
/// - `"structural"`: annotation + in-degree cross-cutting detection.
/// - `"hybrid"`: structural → package → embed → llm (if `llm` provided) in sequence.
/// - `"llm"`: LLM-only (requires `llm` to be `Some`; falls back to package if absent).
///
/// Returns `Err` only when `"hybrid"` or `"embed"` is requested and the embedding model
/// fails to load (e.g. model file not found).
pub fn build_feature_strategy(
    kind: crate::discover::FeatureStrategyKind,
    pkg_cfg: PackageConfig,
    llm: Option<FeatureLlmOptions>,
) -> Result<Box<dyn FeatureStrategy>> {
    use crate::discover::FeatureStrategyKind;
    match kind {
        FeatureStrategyKind::Structural => Ok(Box::new(StructuralStrategy::new(
            StructuralConfig::default(),
        ))),
        FeatureStrategyKind::Llm => {
            if let Some(opts) = llm {
                let caller = Arc::new(EngineLlmCaller {
                    adapter: opts.adapter,
                    api_key: opts.api_key,
                    model: opts.model,
                    max_tokens: opts.max_tokens,
                    timeout_secs: opts.timeout_secs,
                });
                Ok(Box::new(LlmStrategy::new(
                    caller,
                    LlmConfig::default(),
                    opts.prior_artifact,
                )))
            } else {
                tracing::warn!(
                    "--feature-strategy llm requires LLM config; falling back to package"
                );
                Ok(Box::new(PackageStrategy::new(pkg_cfg)))
            }
        }
        FeatureStrategyKind::Hybrid => {
            let embedder = load_embedder()?;
            let structural = Box::new(StructuralStrategy::new(StructuralConfig::default()));
            let package = Box::new(PackageStrategy::new(pkg_cfg));
            let embed = Box::new(EmbedStrategy::new(embedder, EmbedConfig::default()));

            let catch_all = vec!["shared".into(), "core".into(), "common".into()];

            let mut strategies: Vec<Box<dyn FeatureStrategy>> = vec![structural, package, embed];

            if let Some(opts) = llm {
                let caller = Arc::new(EngineLlmCaller {
                    adapter: opts.adapter,
                    api_key: opts.api_key,
                    model: opts.model,
                    max_tokens: opts.max_tokens,
                    timeout_secs: opts.timeout_secs,
                });
                strategies.push(Box::new(LlmStrategy::new(
                    caller,
                    LlmConfig {
                        batch_size: 18,
                        catch_all_features: catch_all.clone(),
                    },
                    opts.prior_artifact,
                )));
            }

            Ok(Box::new(HybridStrategy::new(strategies, catch_all)))
        }
        FeatureStrategyKind::Embed => {
            // The embed clusterer needs Postgres + Leiden orchestration, which happens in
            // `discover::run_discover_core` before this builder is reached. If we get here the
            // pg path was unavailable, so fall back to package.
            tracing::warn!(
                "--feature-strategy embed reached the generic builder (no pg path) — falling back to package"
            );
            Ok(Box::new(PackageStrategy::new(pkg_cfg)))
        }
        FeatureStrategyKind::Package => Ok(Box::new(PackageStrategy::new(pkg_cfg))),
    }
}

fn load_embedder() -> Result<Arc<dyn Embedder>> {
    let model = EmbedModel::load(EmbedModelKind::MiniLm)?;
    Ok(Arc::new(EngineEmbedder { model }))
}
