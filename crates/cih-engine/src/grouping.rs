use std::sync::Arc;

use anyhow::Result;
use cih_embed::{EmbedModel, EmbedModelKind};
use cih_grouping::{
    Embedder, EmbedConfig, EmbedStrategy, FeatureStrategy, HybridStrategy, PackageConfig,
    PackageStrategy, StructuralConfig, StructuralStrategy,
};

/// `cih-embed::EmbedModel` wrapped as a `cih-grouping::Embedder`.
struct EngineEmbedder {
    model: EmbedModel,
}

impl Embedder for EngineEmbedder {
    fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.model.embed(texts)
    }
}

/// Build the feature classification strategy selected by the `--feature-strategy` flag.
///
/// - `"package"` (default): fast file-path heuristic, zero cost.
/// - `"structural"`: annotation + in-degree cross-cutting detection.
/// - `"hybrid"`: structural → package → embed in sequence.
///
/// Returns `Err` only when `"hybrid"` or `"embed"` is requested and the embedding model
/// fails to load (e.g. model file not found). In that case the caller can fall back to
/// `"package"`.
pub fn build_feature_strategy(
    kind: &str,
    pkg_cfg: PackageConfig,
) -> Result<Box<dyn FeatureStrategy>> {
    match kind {
        "structural" => Ok(Box::new(StructuralStrategy::new(StructuralConfig::default()))),
        "hybrid" => {
            let embedder = load_embedder()?;
            let structural = Box::new(StructuralStrategy::new(StructuralConfig::default()));
            let package = Box::new(PackageStrategy::new(pkg_cfg));
            let embed = Box::new(EmbedStrategy::new(embedder, EmbedConfig::default()));
            Ok(Box::new(HybridStrategy::new(
                vec![structural, package, embed],
                vec!["shared".into(), "core".into(), "common".into()],
            )))
        }
        _ => Ok(Box::new(PackageStrategy::new(pkg_cfg))),
    }
}

fn load_embedder() -> Result<Arc<dyn Embedder>> {
    let model = EmbedModel::load(EmbedModelKind::MiniLm)?;
    Ok(Arc::new(EngineEmbedder { model }))
}
