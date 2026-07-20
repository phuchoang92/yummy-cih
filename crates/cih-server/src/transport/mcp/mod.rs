//! MCP tool adapters grouped by capability.

use rmcp::handler::server::router::tool::ToolRouter;

use crate::app::CihServer;

mod admin;
mod crossrepo;
#[cfg(test)]
mod dispatch_tests;
mod files;
mod overview;
mod testing;
mod wiki;

/// Assemble every split MCP adapter router in one place so server construction
/// and router-contract tests cannot drift onto different module lists.
pub(crate) fn router() -> ToolRouter<CihServer> {
    CihServer::files_router()
        + CihServer::crossrepo_router()
        + CihServer::testing_router()
        + CihServer::wiki_router()
        + CihServer::overview_router()
        + CihServer::admin_router()
}
