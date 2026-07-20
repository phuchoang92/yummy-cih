//! CIH MCP server library.
//!
//! The public surface is deliberately small: [`run`] (the server entry point
//! used by the `cih-server` binary) plus the modules exercised by integration
//! tests (`args`, `browser`, `search`, `utils`, `viz`, `wiki`).
//! Everything else is crate-private wiring.

mod application;
mod bootstrap;
mod domain;
mod infrastructure;
mod ports;
mod transport;

pub mod utils;
pub mod viz;

#[doc(hidden)]
pub mod scale_bench;

pub(crate) mod config;
pub(crate) mod layout;

pub use bootstrap::run;

/// Compatibility exports for MCP argument DTOs used by downstream clients.
pub mod args {
    pub use crate::transport::mcp::args::*;
}

/// Compatibility exports for graph-browser helpers used by downstream tests.
pub mod browser {
    pub use crate::transport::http::browser::{
        bounded_depth, limit_or_default, overview_limit, parse_graph_direction, render_flow_graph,
        INDEX_HTML, OVERVIEW_DEFAULT_EDGES, OVERVIEW_DEFAULT_NODES, OVERVIEW_MAX_EDGES,
        OVERVIEW_MAX_NODES,
    };
}

/// Stable public search helpers; implementation lives in infrastructure.
pub mod search {
    pub use crate::infrastructure::search_provider::query_limit;
}

/// Stable public wiki parsing/search helpers; implementation lives in infrastructure.
pub mod wiki {
    pub use crate::infrastructure::wiki_repository::{
        load_wiki_index, make_snippet, strip_front_matter, PageMeta, WikiFacets, WikiHit,
        WikiIndex, DEFAULT_LIMIT, MAX_LIMIT, SNIPPET_MAX_CHARS,
    };
}
