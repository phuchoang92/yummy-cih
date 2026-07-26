//! Semantic embedding support for CIH graph nodes.
//!
//! The public helpers in this crate are intentionally split between pure text
//! preparation (`chunk_text`, `content_hash`, `embeddable_nodes`) and the
//! operational path (`EmbedStore`) that talks to fastembed and pgvector.

mod chunker;
mod inference;
mod model;
mod store;
mod strip;
mod text;

pub use chunker::{chunk_text, Chunk};
pub use inference::{
    EmbedInferenceConfig, EmbedInferenceError, EmbedInferenceMetricsSnapshot,
    DEFAULT_EMBED_INFERENCE_MAX_CONCURRENT, DEFAULT_EMBED_INFERENCE_QUEUE_TIMEOUT_MS,
    DEFAULT_EMBED_INFERENCE_TIMEOUT_MS,
};
pub use model::{EmbedModel, EmbedModelKind};
pub use store::{EmbedStore, EmbedSummary, NodeVector, SemanticHit};
pub use strip::strip_java_body;
pub use text::{content_hash, embeddable_nodes, embedding_text, is_embeddable_kind, source_bodies};
