//! ACP-native MCP-over-ACP hosting for the ee MCP proxy (Phase 6b).
//!
//! # Architecture
//!
//! The ACP agent is the MCP client; this module hosts the MCP **server**
//! side of the `ee` proxy inside the agent connection:
//!
//! - `session/new` advertises an ACP `McpServer::Acp` entry named `ee`
//!   (instead of the stdio `ee --mcp-proxy` entry) when the proxy is
//!   configured and the agent advertised `mcp_capabilities.acp`.
//! - `mcp/connect`, `mcp/message`, and `mcp/disconnect` requests from the
//!   agent are handled with the official SDK wire types (the "official
//!   method metadata"), enforcing strict ordering and identity rules.
//! - Inner MCP messages are served by the existing
//!   [`ee_mcp::EeMcpProxy`] (`rmcp::ServerHandler`) over an in-process
//!   transport, so the `ee_*` tool definitions, argument validation,
//!   absolute-path rules, terminal env redaction, and result mapping are
//!   reused verbatim.
//! - Tool execution routes through the same [`ClientRequestHandler`] as
//!   direct ACP client methods, so bridge approval prompts, `ApprovalPolicy`,
//!   buffer/edit/save semantics, terminal limits, and diagnostics redaction
//!   apply unchanged.
//!
//! # SDK gap (documented, tested)
//!
//! Upstream `agent-client-protocol-rmcp` requires `rmcp ^2.x`, incompatible
//! with this workspace's `rmcp 3.x` (see `ee-agent-protocol::mcp_over_acp`).
//! Until upstream publishes an `rmcp 3.x`-compatible release, this module is
//! the minimal local adapter: it uses the SDK's wire types and the rmcp
//! server loop, and owns only (a) the transport plumbing between the SDK
//! dispatch and rmcp, and (b) the sync backend bridge into the host handler.
//! No ACP or MCP wire structs are handrolled.
//!
//! # Lifecycle
//!
//! Logical MCP connections are per agent connection (the wire protocol has
//! no session id in `mcp/*`), keyed by connection id, and each records its
//! server id.  All connections close on turn cancel (`session/cancel`),
//! session close, agent disconnect, and app shutdown.  A frame cap matching
//! the stdio proxy cap fails closed (no partial parse).

pub(crate) use std::collections::HashMap;
pub(crate) use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
pub(crate) use std::sync::{Arc, Mutex};
pub(crate) use std::time::Duration;

pub(crate) use ee_agent_protocol::{
    Agent as AgentRole, ConnectMcpRequest, ConnectMcpResponse, ConnectionTo, DisconnectMcpRequest,
    DisconnectMcpResponse, Error as RpcError, JsonRpcResponse, KillTerminalRequest,
    McpConnectionId, McpServerAcpId, MessageMcpNotification, MessageMcpRequest, MessageMcpResponse,
    ReleaseTerminalRequest, Responder, SessionId, TerminalId, TerminalOutputRequest,
    WaitForTerminalExitRequest,
};
pub(crate) use ee_mcp::{
    BrowserRunRequest, BrowserRunResult, ChangedFilesResult, CodeActionsResult, DiagnosticsResult,
    DocumentSymbolsResult, EditTextResult, EeMcpProxy, EeProxyBackend, FetchUrlRequest,
    FetchUrlResult, FileDependencyMapResult, FilesystemResult, GitDiffResult, GitStatusResult,
    ListDirectoryAllResult, ListDirectoryResult, OpenBuffersResult, ProjectInstructionsResult,
    ProxyToolError, ReferencesResult, RenamePreviewResult, ReviewContextResult,
    SearchFilesAllResult, SearchFilesResult, SearchTextResult, SessionNoteResult,
    SessionNotesResult, SymbolDependencyMapResult, TerminalOutputResult, TerminalWaitResult,
    TextEdit, WebSearchRequest, WebSearchResult, WorkspaceEditResult, WorkspaceFact,
    WorkspaceFactMutationResult, WorkspaceFactsResult, WorkspaceRootsResult,
};
pub(crate) use rmcp::model::{
    JsonRpcMessage, RequestId, ServerNotification, ServerRequest, ServerResult,
};
pub(crate) use rmcp::service::{RoleServer, RxJsonRpcMessage, TxJsonRpcMessage};
pub(crate) use rmcp::transport::Transport;
pub(crate) use sha2::{Digest, Sha256};
pub(crate) use tokio::sync::{mpsc, oneshot};
pub(crate) use tokio_util::sync::CancellationToken;

pub(crate) use crate::error::AgentError;
pub(crate) use crate::inbound::{
    ClientRequest, ClientRequestHandler, ClientRequestResponse, HandlerCapabilities, ProxyTextEdit,
    WorkspaceMemoryMutationOperation,
};
pub(crate) use crate::process::AgentProcess;
pub(crate) use crate::session::ThreadShared;
pub(crate) use crate::turn_evidence::TurnKey;
pub(crate) use crate::workspace_memory::WorkspaceMemoryHost;
pub(crate) use crate::workspace_verified_facts::derive_workspace_verified_fact_candidates;

/// The outbound message type the rmcp serve loop hands to the transport
/// (server→client traffic; for the ee proxy always a response to an inner
/// request).  Written out longhand because matching through the
/// `TxJsonRpcMessage` alias cannot infer the role parameter.
type TxServerMessage = JsonRpcMessage<ServerRequest, ServerResult, ServerNotification>;

/// Cap on one MCP-over-ACP inner frame (the `method` + `params` payload of a
/// `mcp/message`), in bytes.  Matches the stdio proxy cap
/// (`ee-cli` `PROXY_MAX_FRAME_BYTES`); oversized frames fail closed and
/// close the logical connection (no partial parse).
pub const MCP_OVER_ACP_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// How long a proxy tool call may wait for its approval round trip before
/// the serve side gives up (mirrors the stdio proxy socket timeout).
const MCP_OVER_ACP_APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Tool exposure profile for one connection-owned ee MCP proxy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EeProxyToolProfile {
    /// Normal agent session: expose every handler-supported tool.
    #[default]
    Full,
    /// External critic: expose only approval-free, non-terminal read tools.
    CriticReadOnly,
}

/// How the ee proxy is exposed to one agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EeProxyMode {
    /// ACP-native: `session/new` carried an `McpServer::Acp` `ee` entry and
    /// the agent drives it with `mcp/connect` / `mcp/message` /
    /// `mcp/disconnect`.
    AcpNative,
    /// Stdio fallback: `session/new` carried the `ee --mcp-proxy` stdio
    /// entry because the agent did not advertise MCP-over-ACP support.
    StdioFallback,
    /// No ee proxy was advertised (proxy mode off).
    Disabled,
}

impl std::fmt::Display for EeProxyMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AcpNative => write!(f, "acp-native"),
            Self::StdioFallback => write!(f, "stdio fallback"),
            Self::Disabled => write!(f, "disabled"),
        }
    }
}
/// One job queued for the per-connection proxy executor: a proxy tool call
/// forwarded to the shared [`ClientRequestHandler`] (approval path), with
/// the result delivered back over a std channel (the serve thread blocks on
/// it; the executor runs on the host runtime).
struct ProxyJob {
    request: ClientRequest,
    cancel: CancellationToken,
    reply: std::sync::mpsc::Sender<ClientRequestResult>,
}

type ClientRequestResult = Result<ClientRequestResponse, AgentError>;

fn proxy_value<T: serde::de::DeserializeOwned>(
    response: ClientRequestResponse,
    operation: &str,
) -> Result<T, ProxyToolError> {
    match response {
        ClientRequestResponse::ProxyValue(value) => {
            serde_json::from_value(value).map_err(|error| ProxyToolError {
                message: format!("proxy {operation} returned invalid payload: {error}"),
                is_permission_denied: false,
            })
        }
        _ => Err(ProxyToolError {
            message: format!("proxy {operation} returned an unexpected response"),
            is_permission_denied: false,
        }),
    }
}

/// Drives proxy tool calls through the shared handler on the host runtime.
///
/// The capability gate mirrors [`crate::connection::dispatch_client_request`]:
/// a handler that did not advertise a capability fails closed before the
/// handler is invoked.
async fn proxy_executor(
    handler: Arc<dyn ClientRequestHandler>,
    capabilities: HandlerCapabilities,
    mut jobs: mpsc::UnboundedReceiver<ProxyJob>,
) {
    while let Some(job) = jobs.recv().await {
        let method = job.request.method();
        let result = if capabilities.supports_request(&job.request) {
            tokio::select! {
                () = job.cancel.cancelled() => Err(AgentError::Cancelled),
                result = handler.handle(job.request) => result,
            }
        } else {
            Err(AgentError::CapabilityUnsupported { method: method.to_string() })
        };
        let _ = job.reply.send(result);
    }
}

pub(crate) use backend::HostProxyBackend;
pub(crate) use registry::McpOverAcpRegistry;
pub(super) use transport::{LogicalConnection, McpOverAcpTransport, PendingReply};

mod backend;
mod registry;
mod tools;
mod transport;

#[cfg(test)]
mod tests;
