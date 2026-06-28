//! Semantic embedding support for CIH graph nodes.
//!
//! The public helpers in this crate are intentionally split between pure text
//! preparation (`chunk_text`, `content_hash`, `embeddable_nodes`) and the
//! operational path (`EmbedStore`) that talks to fastembed and pgvector.

mod chunker;
mod model;
mod store;
mod strip;
mod text;

pub use chunker::{chunk_text, Chunk};
pub use model::{EmbedModel, EmbedModelKind};
pub use store::{EmbedStore, EmbedSummary, SemanticHit};
pub use strip::strip_java_body;
pub use text::{content_hash, embeddable_nodes, embedding_text, is_embeddable_kind, source_bodies};

