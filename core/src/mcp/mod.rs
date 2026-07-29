//! The MCP server the daemon exposes on its AF_UNIX socket: the structural
//! tool surface an agent sees, plus the plumbing that binds one accepted
//! connection to one `rmcp` session.
//!
//! Every tool is registered with its real name, description and parameter
//! schema, and each one's answer lives in a module of its own next to this
//! one - the schemas were registered up front, before the handlers existed,
//! so a client could be wired against the finished surface rather than watch
//! tools appear one by one; this file has stayed pure router wiring since.
//!
//! Why `rmcp` (and therefore tokio) only here: the rest of the daemon - SQLite,
//! the plugin bridge, the watcher - is plainly synchronous and has no reason
//! not to be. `daemon::run` enters a small runtime for its accept loop alone,
//! so async stops at this module's front door; handlers below hold the same
//! plain `Mutex` guards the synchronous code always has.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::net::UnixStream;

use crate::daemon::plugin::PluginProcess;
use crate::graph::pagination::Direction;
use crate::protocol::types::Position;

mod find_callers_callees;
mod find_definition;
mod find_implementations;
mod find_references;
mod get_dependencies;
mod get_file_outline;
mod tool_result;

/// Serves one accepted socket connection as an MCP session until the peer
/// disconnects. One session per connection, and the shim opens exactly one
/// connection per MCP client, so a client's session dies with its shim.
pub async fn serve_connection(
    stream: UnixStream,
    conn: Arc<Mutex<Connection>>,
    plugin: Arc<PluginProcess>,
) -> Result<()> {
    let service = GMeshMcpServer::new(conn, plugin)
        .serve(stream)
        .await
        .context("MCP initialization failed")?;
    service.waiting().await.context("MCP session task failed")?;
    Ok(())
}

/// The structural query surface, backed by the project's index and the
/// language plugin.
///
/// Every handler answers out of `conn` alone; `plugin` is held but not read
/// yet, because the tool that needs it - a query the index cannot answer
/// without asking the language server - is still ahead of the MVP surface.
#[derive(Clone)]
pub struct GMeshMcpServer {
    conn: Arc<Mutex<Connection>>,
    #[allow(dead_code)]
    plugin: Arc<PluginProcess>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl GMeshMcpServer {
    pub fn new(conn: Arc<Mutex<Connection>>, plugin: Arc<PluginProcess>) -> Self {
        Self { conn, plugin, tool_router: Self::tool_router() }
    }

    #[tool(
        name = "find_definition",
        description = "Find where a symbol is defined. Give either a symbol name, or a file path with a cursor position to resolve the symbol under it."
    )]
    async fn find_definition(
        &self,
        params: Parameters<FindDefinitionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        find_definition::handle(&self.conn, params.0)
    }

    #[tool(
        name = "find_references",
        description = "List every place a symbol is referenced, across the whole project."
    )]
    async fn find_references(
        &self,
        params: Parameters<SymbolQueryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        find_references::handle(&self.conn, params.0)
    }

    #[tool(name = "find_callers", description = "List the functions that call the given function.")]
    async fn find_callers(&self, params: Parameters<SymbolQueryParams>) -> Result<CallToolResult, ErrorData> {
        find_callers_callees::handle_callers(&self.conn, params.0)
    }

    #[tool(name = "find_callees", description = "List the functions the given function calls.")]
    async fn find_callees(&self, params: Parameters<SymbolQueryParams>) -> Result<CallToolResult, ErrorData> {
        find_callers_callees::handle_callees(&self.conn, params.0)
    }

    #[tool(
        name = "find_implementations",
        description = "List the types that implement or extend the given interface, base class or abstract type."
    )]
    async fn find_implementations(
        &self,
        params: Parameters<SymbolQueryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        find_implementations::handle(&self.conn, params.0)
    }

    #[tool(
        name = "get_file_outline",
        description = "List the top-level symbols a file declares, in source order."
    )]
    async fn get_file_outline(
        &self,
        params: Parameters<GetFileOutlineParams>,
    ) -> Result<CallToolResult, ErrorData> {
        get_file_outline::handle(&self.conn, params.0)
    }

    #[tool(
        name = "get_dependencies",
        description = "Walk the import graph out of (or into) a file or module, up to a bounded depth and fan-out."
    )]
    async fn get_dependencies(
        &self,
        params: Parameters<GetDependenciesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        get_dependencies::handle(&self.conn, params.0)
    }
}

// `router = self.tool_router` on purpose: the attribute's default is
// `Self::tool_router()`, which rebuilds the whole router - all seven schemas -
// on every single tools/list and tools/call.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for GMeshMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("g-mesh", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Structural code-graph queries over this project's index. Prefer these over \
                 grepping when you need definitions, references, call edges or imports.",
            )
    }
}

/// A symbol is addressable either by name or by where the cursor sits, because
/// an agent reading code has one or the other, rarely both.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindDefinitionParams {
    /// Name of the symbol to look up.
    pub symbol_name: Option<String>,
    /// Project-relative path of the file the cursor is in.
    pub file_path: Option<String>,
    /// Cursor position within `file_path`, used to resolve the symbol under it.
    pub position: Option<Position>,
    /// Opaque cursor from a previous page of results.
    pub cursor: Option<String>,
}

/// find_references/find_callers/find_callees/find_implementations differ only
/// in which edges they walk, never in what the caller has to supply - so they
/// share one parameter shape instead of four identical ones.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SymbolQueryParams {
    /// Id of the anchor symbol, as returned by `find_definition`.
    pub symbol_id: String,
    /// Opaque cursor from a previous page of results.
    pub cursor: Option<String>,
    /// Maximum results to return in this call (default 20, capped at 200).
    /// Raise this to fetch a larger result set in one call instead of
    /// paging through it with `cursor`.
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFileOutlineParams {
    /// Project-relative path of the file to outline.
    pub file_path: String,
    /// Opaque cursor from a previous page of results.
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetDependenciesParams {
    /// Project-relative path of the file to start from.
    pub file_path: Option<String>,
    /// Id of the module to start from, as an alternative to `file_path`.
    pub module_id: Option<String>,
    /// `Outgoing` for what this file imports, `Incoming` for what imports it.
    pub direction: Direction,
    /// How many import hops to follow.
    pub max_depth: Option<u32>,
    /// How many dependencies to expand per node before truncating.
    pub max_fanout: Option<u32>,
    /// Opaque token from a previous, truncated traversal.
    pub resume_token: Option<String>,
}
