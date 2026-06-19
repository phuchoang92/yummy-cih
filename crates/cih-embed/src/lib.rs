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
pub use text::{content_hash, embeddable_nodes, embedding_text, is_embeddable_kind};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_produces_one_chunk() {
        let chunks = chunk_text("short method summary", 4000, 500);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "short method summary");
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 1);
    }

    #[test]
    fn long_text_chunks_with_overlap() {
        let text = "a".repeat(10_000);
        let chunks = chunk_text(&text, 4_000, 500);

        assert!(chunks.len() >= 3);
        assert_eq!(chunks[0].start_byte, 0);
        assert_eq!(chunks[1].start_byte, 3_500);
        assert_eq!(chunks[0].end_byte, 4_000);
    }

    #[test]
    fn content_hash_is_stable_and_node_scoped() {
        let first = content_hash(
            "Method:com.acme.OwnerService#findAll/0",
            "OwnerService findAll",
        );
        let second = content_hash(
            "Method:com.acme.OwnerService#findAll/0",
            "OwnerService findAll",
        );
        let third = content_hash(
            "Method:com.acme.OwnerService#save/1",
            "OwnerService findAll",
        );

        assert_eq!(first, second);
        assert_ne!(first, third);
    }
}
