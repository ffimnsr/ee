//! MCP stdio server for agent drivers.
//!
//! A second thin surface over the same [`RemoteHost`] core. One MCP server
//! process owns one host and its own agent connections; drivers never share a
//! process. Wire handling is rmcp's; ee-owned code is the tool surface,
//! argument validation, result caps, and dispatch into the host.

mod args;
mod tools;

use std::borrow::Cow;
use std::future::Future;

use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, DiscoverResult, ErrorCode, ErrorData, Implementation,
    InitializeRequestParams, InitializeResult, ListToolsResult, PaginatedRequestParams,
    ProtocolVersion, ServerCapabilities, Tool,
};
use rmcp::service::{MaybeSendFuture, RequestContext, RoleServer};

use crate::error::RemoteError;
use crate::host::RemoteHost;

pub use args::{MAX_TOOL_RESULT_BYTES, complete_json, require_arguments, require_no_arguments};

/// The single MCP protocol version this server implements.
const SUPPORTED_PROTOCOL_VERSIONS: &[ProtocolVersion] = &[ProtocolVersion::V_2026_07_28];

/// MCP server identity sent on `initialize`.
pub const MCP_SERVER_NAME: &str = "ee-agent";

/// An MCP server exposing remote agent control tools.
#[derive(Clone)]
pub struct RemoteMcpServer {
    host: RemoteHost,
}

impl std::fmt::Debug for RemoteMcpServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("RemoteMcpServer").field("host", &self.host).finish()
    }
}

impl RemoteMcpServer {
    /// Creates a server over one host.
    #[must_use]
    pub fn new(host: RemoteHost) -> Self {
        Self { host }
    }

    /// The host this server drives.
    #[must_use]
    pub fn host(&self) -> &RemoteHost {
        &self.host
    }

    fn capabilities() -> ServerCapabilities {
        ServerCapabilities::builder().enable_tools().build()
    }

    fn server_info() -> Implementation {
        Implementation::new(MCP_SERVER_NAME, env!("CARGO_PKG_VERSION"))
            .with_title("ee agent remote control")
    }

    fn tools(&self) -> Vec<Tool> {
        tools::tool_list(&self.host)
    }
}

impl ServerHandler for RemoteMcpServer {
    fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<InitializeResult, ErrorData>> + MaybeSendFuture + '_ {
        context.peer.set_peer_info(request.clone());
        if request.protocol_version != ProtocolVersion::V_2026_07_28 {
            return std::future::ready(Err(ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                format!("unsupported protocol version: {}", request.protocol_version),
                None,
            )));
        }
        std::future::ready(Ok(InitializeResult::new(Self::capabilities())
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
            .with_server_info(Self::server_info())))
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(SUPPORTED_PROTOCOL_VERSIONS)
    }

    fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<DiscoverResult, ErrorData>> + MaybeSendFuture + '_ {
        std::future::ready(Ok(DiscoverResult::new(
            SUPPORTED_PROTOCOL_VERSIONS.to_vec(),
            Self::capabilities(),
        )))
    }

    fn ping(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), ErrorData>> + MaybeSendFuture + '_ {
        std::future::ready(Ok(()))
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + MaybeSendFuture + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(self.tools())))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + MaybeSendFuture + '_ {
        let host = self.host.clone();
        async move {
            let timeout = host.config().request_timeout;
            match tokio::time::timeout(timeout, tools::dispatch(&host, &request)).await {
                Ok(result) => result,
                Err(_) => Ok(tools::timeout_response(request.name.as_ref(), timeout)),
            }
        }
    }
}

/// Serves the MCP protocol over stdin/stdout until the client closes.
///
/// # Errors
///
/// Returns a typed remote error when the host is not configured or the
/// transport handshake fails.
pub async fn serve_stdio(host: RemoteHost) -> Result<(), RemoteError> {
    host.start()?;
    let service = RemoteMcpServer::new(host.clone());
    let transport = rmcp::transport::io::stdio();
    let running = rmcp::serve_server(service, transport)
        .await
        .map_err(|error| RemoteError::internal(format!("MCP handshake failed: {error}")))?;
    let result = running.waiting().await;
    host.shutdown().await;
    result.map_err(|error| RemoteError::internal(format!("MCP server task failed: {error}")))?;
    Ok(())
}
