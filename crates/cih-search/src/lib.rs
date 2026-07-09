//! Lightweight lexical search over CIH graph nodes.
//!
//! This crate is deliberately storage-free: callers build a BM25 index from
//! `GraphArtifacts` nodes, run lexical search, and optionally fuse those hits
//! with semantic hits from `cih-embed`.

mod bm25;
mod rrf;
mod tokenize;

use cih_core::NodeKind;

pub use bm25::{IndexedDoc, SearchIndex};
pub use rrf::{rrf_merge, SearchHit, RRF_K};
pub use tokenize::tokenize;

pub fn is_searchable_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class
            | NodeKind::Interface
            | NodeKind::Enum
            | NodeKind::Record
            | NodeKind::Annotation
            | NodeKind::Method
            | NodeKind::Constructor
            | NodeKind::Field
            | NodeKind::Route
            | NodeKind::DbTable
            | NodeKind::IntegrationRoute
            | NodeKind::MessageDestination
    )
}
