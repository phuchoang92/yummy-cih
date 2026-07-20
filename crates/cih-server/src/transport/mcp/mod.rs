//! MCP tool adapters grouped by capability.

use rmcp::handler::server::router::tool::ToolRouter;

pub(crate) use server::CihServer;

pub mod args;
#[cfg(test)]
mod dispatch_tests;
mod error;
pub(crate) mod resources;
mod server;
mod tools;

/// Assemble every split MCP adapter router in one place so server construction
/// and router-contract tests cannot drift onto different module lists.
pub(crate) fn router() -> ToolRouter<CihServer> {
    CihServer::graph_router()
        + CihServer::search_router()
        + CihServer::repository_admin_router()
        + CihServer::files_router()
        + CihServer::crossrepo_router()
        + CihServer::testing_router()
        + CihServer::wiki_router()
        + CihServer::overview_router()
        + CihServer::admin_router()
}
