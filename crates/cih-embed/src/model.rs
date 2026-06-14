use std::sync::Mutex;

use anyhow::{anyhow, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbedModelKind {
    MiniLm,
    BgeSmall,
}

impl EmbedModelKind {
    pub fn parse(input: &str) -> Result<Self> {
        match input {
            "all-minilm-l6-v2" | "minilm" | "mini-lm" => Ok(Self::MiniLm),
            "bge-small-en-v1.5" | "bge-small" | "bge" => Ok(Self::BgeSmall),
            other => Err(anyhow!(
                "unknown embedding model '{other}' (use all-minilm-l6-v2 or bge-small-en-v1.5)"
            )),
        }
    }

    pub fn fastembed_model(self) -> EmbeddingModel {
        match self {
            Self::MiniLm => EmbeddingModel::AllMiniLML6V2,
            Self::BgeSmall => EmbeddingModel::BGESmallENV15,
        }
    }

    pub fn dimension(self) -> usize {
        match self {
            Self::MiniLm => 384,
            Self::BgeSmall => 384,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::MiniLm => "all-minilm-l6-v2",
            Self::BgeSmall => "bge-small-en-v1.5",
        }
    }
}

pub struct EmbedModel {
    kind: EmbedModelKind,
    inner: Mutex<TextEmbedding>,
}

impl EmbedModel {
    pub fn load(kind: EmbedModelKind) -> Result<Self> {
        let options = InitOptions::new(kind.fastembed_model()).with_show_download_progress(false);
        let inner = TextEmbedding::try_new(options)?;
        Ok(Self {
            kind,
            inner: Mutex::new(inner),
        })
    }

    pub fn kind(&self) -> EmbedModelKind {
        self.kind
    }

    pub fn dimension(&self) -> usize {
        self.kind.dimension()
    }

    pub fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let model = self
            .inner
            .lock()
            .map_err(|_| anyhow!("embedding model lock poisoned"))?;
        let embeddings = model.embed(texts.to_vec(), None)?;
        Ok(embeddings)
    }
}
