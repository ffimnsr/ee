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

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_protocol::{
    Agent as AgentRole, ConnectMcpRequest, ConnectMcpResponse, ConnectionTo, DisconnectMcpRequest,
    DisconnectMcpResponse, Error as RpcError, JsonRpcResponse, McpConnectionId, McpServerAcpId,
    MessageMcpNotification, MessageMcpRequest, MessageMcpResponse, Responder, SessionId,
};
use ee_mcp::{
    CodeActionsResult, DiagnosticsResult, DocumentSymbolsResult, EditTextResult, EeMcpProxy,
    EeProxyBackend, ListDirectoryAllResult, ListDirectoryResult, OpenBuffersResult, ProxyToolError,
    ReferencesResult, RenamePreviewResult, SearchFilesAllResult, SearchFilesResult,
    SearchTextResult, TextEdit, WorkspaceEditResult, WorkspaceRootsResult,
};
use rmcp::model::{JsonRpcMessage, RequestId, ServerNotification, ServerRequest, ServerResult};
use rmcp::service::{RoleServer, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::error::AgentError;
use crate::inbound::{
    ClientRequest, ClientRequestHandler, ClientRequestResponse, HandlerCapabilities, ProxyTextEdit,
};
use crate::process::AgentProcess;

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
    reply: std::sync::mpsc::Sender<ClientRequestResult>,
}

type ClientRequestResult = Result<ClientRequestResponse, AgentError>;

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
        let result = if capabilities.supports(method) {
            handler.handle(job.request).await
        } else {
            Err(AgentError::CapabilityUnsupported { method: method.to_string() })
        };
        let _ = job.reply.send(result);
    }
}

/// `EeProxyBackend` for the in-process MCP-over-ACP path.
///
/// Tool calls are forwarded to the connection's [`ClientRequestHandler`]
/// through the executor, so approvals, buffer semantics, terminal limits,
/// and redaction are exactly the direct ACP client method paths.  The
/// synchronous backend methods block the *serve thread* only; the executor
/// runs on the host runtime and the UI answers approvals on its own thread.
struct HostProxyBackend {
    jobs: mpsc::UnboundedSender<ProxyJob>,
    process: Arc<Mutex<Option<AgentProcess>>>,
}

impl HostProxyBackend {
    fn call(&self, request: ClientRequest) -> Result<ClientRequestResponse, ProxyToolError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        if self.jobs.send(ProxyJob { request, reply: reply_tx }).is_err() {
            return Err(ProxyToolError {
                message: String::from("agent host is shutting down"),
                is_permission_denied: false,
            });
        }
        match reply_rx.recv_timeout(MCP_OVER_ACP_APPROVAL_TIMEOUT) {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) => Err(ProxyToolError {
                message: error.to_string(),
                is_permission_denied: matches!(error, AgentError::PermissionDenied { .. }),
            }),
            Err(_) => Err(ProxyToolError {
                message: format!("approval timed out after {MCP_OVER_ACP_APPROVAL_TIMEOUT:?}"),
                is_permission_denied: false,
            }),
        }
    }
}

impl EeProxyBackend for HostProxyBackend {
    fn workspace_roots(&self) -> Result<WorkspaceRootsResult, ProxyToolError> {
        match self.call(ClientRequest::ProxyWorkspaceRoots)? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy workspace_roots returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy workspace_roots returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn list_directory(&self, path: String) -> Result<ListDirectoryResult, ProxyToolError> {
        match self.call(ClientRequest::ProxyListDirectory { path })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy list_directory returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy list_directory returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn list_directory_all(&self, path: String) -> Result<ListDirectoryAllResult, ProxyToolError> {
        match self.call(ClientRequest::ProxyListDirectoryAll { path })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy list_directory returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy list_directory returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn search_files(&self, pattern: String) -> Result<SearchFilesResult, ProxyToolError> {
        match self.call(ClientRequest::ProxySearchFiles { pattern })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy search_files returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy search_files returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn search_files_all(&self, pattern: String) -> Result<SearchFilesAllResult, ProxyToolError> {
        match self.call(ClientRequest::ProxySearchFilesAll { pattern })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy search_files returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy search_files returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn search_text(&self, query: String) -> Result<SearchTextResult, ProxyToolError> {
        match self.call(ClientRequest::ProxySearchText { query })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy search_text returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy search_text returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn search_text_regex(&self, pattern: String) -> Result<SearchTextResult, ProxyToolError> {
        match self.call(ClientRequest::ProxySearchTextRegex { pattern })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy search_text returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy search_text returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn search_text_in_files(
        &self,
        query: String,
        file_glob: String,
    ) -> Result<SearchTextResult, ProxyToolError> {
        match self.call(ClientRequest::ProxySearchTextInFiles { query, file_glob })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!(
                        "proxy search_text_in_files returned invalid payload: {error}"
                    ),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy search_text_in_files returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn replace_text(
        &self,
        path: String,
        old_text: String,
        new_text: String,
    ) -> Result<EditTextResult, ProxyToolError> {
        match self.call(ClientRequest::ProxyReplaceText { path, old_text, new_text })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy replace_text returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy replace_text returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn apply_patch(
        &self,
        path: String,
        edits: Vec<TextEdit>,
    ) -> Result<EditTextResult, ProxyToolError> {
        let edits = edits
            .into_iter()
            .map(|edit| ProxyTextEdit { old_text: edit.old_text, new_text: edit.new_text })
            .collect();
        match self.call(ClientRequest::ProxyApplyPatch { path, edits })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy apply_patch returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy apply_patch returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn create_text_file(
        &self,
        path: String,
        content: String,
    ) -> Result<EditTextResult, ProxyToolError> {
        match self.call(ClientRequest::ProxyCreateTextFile { path, content })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy create_text_file returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy create_text_file returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn overwrite_text_file(
        &self,
        path: String,
        content: String,
    ) -> Result<EditTextResult, ProxyToolError> {
        match self.call(ClientRequest::ProxyOverwriteTextFile { path, content })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy overwrite_text_file returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy overwrite_text_file returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn read_buffer(&self, path: String) -> Result<String, ProxyToolError> {
        match self.call(ClientRequest::ProxyReadBuffer { path })? {
            ClientRequestResponse::ProxyValue(value) => {
                value.as_str().map(ToOwned::to_owned).ok_or_else(|| ProxyToolError {
                    message: String::from("proxy read_buffer returned invalid payload"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy read_buffer returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn read_buffer_lines(
        &self,
        path: String,
        line: u32,
        limit: u32,
    ) -> Result<String, ProxyToolError> {
        match self.call(ClientRequest::ProxyReadBufferLines { path, line, limit })? {
            ClientRequestResponse::ProxyValue(value) => {
                value.as_str().map(ToOwned::to_owned).ok_or_else(|| ProxyToolError {
                    message: String::from("proxy read_buffer_lines returned invalid payload"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy read_buffer_lines returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn open_buffers(&self) -> Result<OpenBuffersResult, ProxyToolError> {
        match self.call(ClientRequest::ProxyOpenBuffers)? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy open_buffers returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy open_buffers returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn get_diagnostics(&self) -> Result<DiagnosticsResult, ProxyToolError> {
        match self.call(ClientRequest::ProxyGetDiagnostics)? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy get_diagnostics returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy get_diagnostics returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn get_file_diagnostics(&self, path: String) -> Result<DiagnosticsResult, ProxyToolError> {
        match self.call(ClientRequest::ProxyGetFileDiagnostics { path })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!(
                        "proxy get_file_diagnostics returned invalid payload: {error}"
                    ),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy get_file_diagnostics returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn document_symbols(&self, path: String) -> Result<DocumentSymbolsResult, ProxyToolError> {
        match self.call(ClientRequest::ProxyDocumentSymbols { path })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy document_symbols returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy document_symbols returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn references(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> Result<ReferencesResult, ProxyToolError> {
        match self.call(ClientRequest::ProxyReferences { path, line, character })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy references returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy references returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn list_code_actions(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> Result<CodeActionsResult, ProxyToolError> {
        match self.call(ClientRequest::ProxyListCodeActions { path, line, character })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy list_code_actions returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy list_code_actions returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn apply_code_action(
        &self,
        path: String,
        action_id: String,
    ) -> Result<EditTextResult, ProxyToolError> {
        match self.call(ClientRequest::ProxyApplyCodeAction { path, action_id })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy apply_code_action returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy apply_code_action returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn format_file(&self, path: String) -> Result<EditTextResult, ProxyToolError> {
        match self.call(ClientRequest::ProxyFormatFile { path })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy format_file returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy format_file returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn preview_rename_symbol(
        &self,
        path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<RenamePreviewResult, ProxyToolError> {
        match self.call(ClientRequest::ProxyPreviewRenameSymbol {
            path,
            line,
            character,
            new_name,
        })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!(
                        "proxy preview_rename_symbol returned invalid payload: {error}"
                    ),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from(
                    "proxy preview_rename_symbol returned an unexpected response",
                ),
                is_permission_denied: false,
            }),
        }
    }

    fn rename_symbol(
        &self,
        path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<WorkspaceEditResult, ProxyToolError> {
        match self.call(ClientRequest::ProxyRenameSymbol { path, line, character, new_name })? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy rename_symbol returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy rename_symbol returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn read_text_file(
        &self,
        path: String,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<String, ProxyToolError> {
        let mut request =
            ee_agent_protocol::ReadTextFileRequest::new(SessionId::new("proxy"), path);
        request.line = line;
        request.limit = limit;
        match self.call(ClientRequest::ReadTextFile(request))? {
            ClientRequestResponse::ReadTextFile(response) => Ok(response.content),
            _ => Err(ProxyToolError {
                message: String::from("proxy read returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn write_text_file(&self, path: String, content: String) -> Result<(), ProxyToolError> {
        let request =
            ee_agent_protocol::WriteTextFileRequest::new(SessionId::new("proxy"), path, content);
        match self.call(ClientRequest::WriteTextFile(request))? {
            ClientRequestResponse::WriteTextFile(_) => Ok(()),
            _ => Err(ProxyToolError {
                message: String::from("proxy write returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn terminal_create(
        &self,
        command: String,
        args: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
    ) -> Result<String, ProxyToolError> {
        let mut request =
            ee_agent_protocol::CreateTerminalRequest::new(SessionId::new("proxy"), command);
        request.args = args;
        if let Some(cwd) = cwd {
            request.cwd = Some(std::path::PathBuf::from(cwd));
        }
        request.env = env
            .into_iter()
            .map(|(name, value)| ee_agent_protocol::EnvVariable::new(name, value))
            .collect();
        match self.call(ClientRequest::CreateTerminal(request))? {
            ClientRequestResponse::CreateTerminal(response) => {
                Ok(response.terminal_id.0.to_string())
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy terminal create returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn diagnostics(&self) -> Vec<String> {
        // Bounded, redacted stderr diagnostics of the agent connection itself
        // (the stderr reader enforces the line/byte caps and the agent host
        // redacts secrets before they reach the UI).
        self.process
            .lock()
            .expect("agent process poisoned")
            .as_ref()
            .map_or_else(Vec::new, AgentProcess::stderr_snapshot)
    }
}

/// What the rmcp serve loop produced for one inner request.
///
/// Boxed so the small `Closed` marker does not carry the large message
/// variant's size.
enum PendingReply {
    Message(Box<TxServerMessage>),
    Closed,
}

/// One logical MCP-over-ACP connection (the server side the agent talks to).
struct LogicalConnection {
    /// Pushes inner MCP messages into the rmcp serve loop (`receive` side).
    rx_tx: mpsc::UnboundedSender<RxJsonRpcMessage<RoleServer>>,
    /// Inner responses from rmcp keyed by inner request id.
    pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<PendingReply>>>>,
    /// Cancels the serve loop on disconnect/close.
    shutdown: CancellationToken,
    /// Next inner MCP request id for this connection.
    next_inner_id: AtomicI64,
}

impl LogicalConnection {
    /// Resolves every pending inner request as closed and cancels the serve
    /// loop; the serve thread then exits on its own (detached).
    fn close(&self) {
        self.shutdown.cancel();
        let pendings: Vec<oneshot::Sender<PendingReply>> = {
            let mut guard = self.pending.lock().expect("pending map poisoned");
            guard.drain().map(|(_, tx)| tx).collect()
        };
        for tx in pendings {
            let _ = tx.send(PendingReply::Closed);
        }
    }
}

/// In-process rmcp server transport bridged to the ACP `mcp/message` flow.
///
/// `receive` pops inner client→server messages pushed by the `mcp/message`
/// handlers; `send` correlates rmcp responses back to the awaiting
/// `mcp/message` responder by inner request id.  rmcp never sends
/// server-initiated messages for the ee proxy (fixed tool list), so every
/// outbound item is a response to an inner request.
struct McpOverAcpTransport {
    rx: mpsc::UnboundedReceiver<RxJsonRpcMessage<RoleServer>>,
    pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<PendingReply>>>>,
}

impl Transport<RoleServer> for McpOverAcpTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let pending = self.pending.clone();
        let item_id = match &item {
            TxServerMessage::Response(response) => Some(response.id.clone()),
            TxServerMessage::Error(error) => error.id.clone(),
            _ => None,
        };
        std::future::ready(match item_id {
            Some(id) => {
                let tx = pending.lock().expect("pending map poisoned").remove(&id);
                match tx {
                    Some(tx) => {
                        let _ = tx.send(PendingReply::Message(Box::new(item)));
                        Ok(())
                    }
                    None => {
                        // Response for a request that was cancelled/closed.
                        tracing::debug!(?id, "dropping stale mcp-over-acp response");
                        Ok(())
                    }
                }
            }
            None => Ok(()),
        })
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        self.rx.recv().await
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }
}

/// Per-agent-connection MCP-over-ACP hosting state.
///
/// Created for every connection; *armed* (accepting `mcp/connect`) only when
/// the host was configured with the ee proxy enabled.  All state is
/// connection-scoped because the `mcp/*` wire methods carry no session id.
pub(crate) struct McpOverAcpRegistry {
    /// The ACP `McpServer::Acp` server id this connection advertises, when
    /// the ee proxy is enabled for it.
    server_id: Option<McpServerAcpId>,
    connections: Mutex<HashMap<McpConnectionId, Arc<LogicalConnection>>>,
    next_connection_id: AtomicU64,
    /// Proxy tool call executor (runs on the host runtime).
    jobs: mpsc::UnboundedSender<ProxyJob>,
    /// Agent stderr capture for the `ee_diagnostics` tool.
    process: Arc<Mutex<Option<AgentProcess>>>,
}

impl McpOverAcpRegistry {
    /// Creates the registry; `enabled` arms MCP-over-ACP for this
    /// connection.  The executor task is spawned on the current runtime and
    /// exits when the connection drops the jobs sender.
    pub(crate) fn new(
        enabled: bool,
        agent_id: &str,
        handler: Arc<dyn ClientRequestHandler>,
        handler_capabilities: HandlerCapabilities,
        process: Arc<Mutex<Option<AgentProcess>>>,
    ) -> Self {
        let (jobs_tx, jobs_rx) = mpsc::unbounded_channel();
        if enabled {
            tokio::spawn(proxy_executor(handler, handler_capabilities, jobs_rx));
        } else {
            drop(jobs_rx);
        }
        let server_id = enabled.then(|| McpServerAcpId::new(format!("ee-mcp-proxy:{agent_id}")));
        Self {
            server_id,
            connections: Mutex::new(HashMap::new()),
            next_connection_id: AtomicU64::new(0),
            jobs: jobs_tx,
            process,
        }
    }

    /// The advertised ACP server id, when the proxy is armed.
    pub(crate) fn server_id(&self) -> Option<&McpServerAcpId> {
        self.server_id.as_ref()
    }

    /// Whether `server_id` is the ee proxy server this connection hosts.
    fn is_our_server(&self, server_id: &McpServerAcpId) -> bool {
        self.server_id.as_ref().is_some_and(|ours| ours == server_id)
    }

    /// Whether a logical connection with `connection_id` exists.
    fn connection(&self, connection_id: &McpConnectionId) -> Option<Arc<LogicalConnection>> {
        self.connections.lock().expect("mcp connections poisoned").get(connection_id).cloned()
    }

    /// Handles `mcp/connect`: validates the server id and the agent's
    /// advertised capability, starts the rmcp serve thread for a fresh
    /// logical connection, and answers with the new connection id.
    ///
    /// Unknown server ids and connections for unadvertised capabilities fail
    /// closed with JSON-RPC invalid params.
    pub(crate) fn handle_connect(
        &self,
        request: ConnectMcpRequest,
        responder: Responder<ConnectMcpResponse>,
        supports_acp: bool,
    ) -> Result<(), RpcError> {
        if !supports_acp {
            return responder.respond_with_error(RpcError::invalid_params().data(
                serde_json::json!({ "reason": "agent did not advertise mcp_capabilities.acp" }),
            ));
        }
        if !self.is_our_server(&request.server_id) {
            return responder.respond_with_error(RpcError::invalid_params().data(
                serde_json::json!({ "reason": "unknown MCP server id", "serverId": request.server_id }),
            ));
        }

        let connection_id = McpConnectionId::new(format!(
            "ee-mcp:{}:{}",
            self.server_id.as_ref().expect("armed server id"),
            self.next_connection_id.fetch_add(1, Ordering::Relaxed),
        ));
        let (rx_tx, rx_rx) = mpsc::unbounded_channel::<RxJsonRpcMessage<RoleServer>>();
        let pending =
            Arc::new(Mutex::new(HashMap::<RequestId, oneshot::Sender<PendingReply>>::new()));
        let shutdown = CancellationToken::new();
        let connection = Arc::new(LogicalConnection {
            rx_tx,
            pending: pending.clone(),
            shutdown: shutdown.clone(),
            next_inner_id: AtomicI64::new(0),
        });

        // The serve thread runs its own current_thread runtime: proxy tool
        // calls block it synchronously while awaiting the host-side
        // approval round trip (never blocking the host runtime).
        let backend = HostProxyBackend { jobs: self.jobs.clone(), process: self.process.clone() };
        let proxy = EeMcpProxy::new(Arc::new(backend));
        let transport = McpOverAcpTransport { rx: rx_rx, pending: pending.clone() };
        let thread_connection = connection.clone();
        let thread_name = format!("ee-mcp-over-acp:{}", connection_id);
        let spawn_result = std::thread::Builder::new().name(thread_name).spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("mcp-over-acp serve runtime");
            runtime.block_on(async move {
                // `serve_server_with_ct` resolves after the initialize
                // handshake and returns the running service; await it so
                // the transport (and its rx channel) stays alive until
                // the connection is cancelled or closed.
                if let Ok(running) =
                    rmcp::service::serve_server_with_ct(proxy, transport, shutdown.clone()).await
                {
                    let _ = running.waiting().await;
                }
                // Resolve any still-pending inner requests so awaiting
                // mcp/message responders never hang.
                thread_connection.close();
            });
        });
        if let Err(error) = spawn_result {
            return responder.respond_with_error(RpcError::internal_error().data(
                serde_json::json!({ "reason": format!("serve thread spawn failed: {error}") }),
            ));
        }
        self.connections
            .lock()
            .expect("mcp connections poisoned")
            .insert(connection_id.clone(), connection);
        responder.respond(ConnectMcpResponse::new(connection_id))
    }

    /// Handles an `mcp/message` request: forwards the inner MCP message into
    /// the logical connection's rmcp serve loop and answers with the inner
    /// response once rmcp produces it.
    ///
    /// Messages for unknown connections (including before `mcp/connect`),
    /// after disconnect, and oversized frames fail closed with invalid
    /// params; oversized frames also close the logical connection.
    pub(crate) fn handle_message(
        &self,
        request: MessageMcpRequest,
        responder: Responder<MessageMcpResponse>,
        cx: &ConnectionTo<AgentRole>,
        supports_acp: bool,
    ) -> Result<(), RpcError> {
        if !supports_acp {
            return responder.respond_with_error(RpcError::invalid_params().data(
                serde_json::json!({ "reason": "agent did not advertise mcp_capabilities.acp" }),
            ));
        }
        let Some(connection) = self.connection(&request.connection_id) else {
            return responder.respond_with_error(RpcError::invalid_params().data(
                serde_json::json!({
                    "reason": "unknown MCP connection (mcp/message before mcp/connect?)",
                    "connectionId": request.connection_id,
                }),
            ));
        };

        // Frame cap: the full `mcp/message` params payload the agent sent
        // (connection id + inner method + inner params), matching the stdio
        // proxy line cap.  Oversized frames fail closed and close the
        // logical connection (no partial parse).
        let frame = serde_json::json!({
            "connectionId": request.connection_id,
            "method": request.method,
            "params": request.params,
        });
        let frame_bytes = serde_json::to_string(&frame).map_or(0, |text| text.len());
        if frame_bytes > MCP_OVER_ACP_MAX_FRAME_BYTES {
            self.close_connection(&request.connection_id);
            return responder.respond_with_error(RpcError::invalid_params().data(
                serde_json::json!({
                    "reason": format!(
                        "mcp/message frame exceeds the {MCP_OVER_ACP_MAX_FRAME_BYTES}-byte cap"
                    ),
                    "connectionId": request.connection_id,
                }),
            ));
        }

        let inner_id = connection.next_inner_id.fetch_add(1, Ordering::Relaxed) + 1;
        let inner_message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": inner_id,
            "method": request.method,
            "params": request.params,
        });
        let Ok(message) = serde_json::from_value::<RxJsonRpcMessage<RoleServer>>(inner_message)
        else {
            return responder.respond_with_error(RpcError::invalid_params().data(
                serde_json::json!({
                    "reason": "inner MCP message is not a valid request",
                    "connectionId": request.connection_id,
                }),
            ));
        };

        let (reply_tx, reply_rx) = oneshot::channel();
        connection
            .pending
            .lock()
            .expect("pending map poisoned")
            .insert(RequestId::Number(inner_id), reply_tx);
        if connection.rx_tx.send(message).is_err() {
            connection
                .pending
                .lock()
                .expect("pending map poisoned")
                .remove(&RequestId::Number(inner_id));
            self.close_connection(&request.connection_id);
            return responder.respond_with_error(RpcError::invalid_params().data(
                serde_json::json!({
                    "reason": "MCP connection is closed",
                    "connectionId": request.connection_id,
                }),
            ));
        }

        let spawned = cx.spawn(async move {
            let outcome = match reply_rx.await {
                Ok(PendingReply::Message(item)) => *item,
                Ok(PendingReply::Closed) | Err(_) => {
                    return responder.respond_with_error(
                        RpcError::request_cancelled()
                            .data(serde_json::json!({ "reason": "MCP connection closed" })),
                    );
                }
            };
            match outcome {
                TxServerMessage::Response(response) => {
                    let value = serde_json::to_value(response.result)
                        .map_err(RpcError::into_internal_error)?;
                    let response = MessageMcpResponse::from_value("mcp/message", value)?;
                    responder.respond(response)
                }
                TxServerMessage::Error(error) => responder.respond_with_error(RpcError::new(
                    error.error.code.0,
                    error.error.message.to_string(),
                )),
                _ => responder.respond_with_error(
                    RpcError::internal_error()
                        .data(serde_json::json!({ "reason": "unexpected inner MCP message" })),
                ),
            }
        });
        if spawned.is_err() {
            // Connection is shutting down; the responder dies with it.
            tracing::debug!("mcp/message responder dropped: connection closing");
        }
        Ok(())
    }

    /// Handles an `mcp/message` notification (inner MCP notification such as
    /// `notifications/initialized`).  Unknown connections are dropped with a
    /// debug log (notifications carry no response channel); oversized frames
    /// fail closed and close the logical connection, like requests.
    pub(crate) fn handle_notification(&self, notification: MessageMcpNotification) {
        let Some(connection) = self.connection(&notification.connection_id) else {
            tracing::debug!(
                connection_id = %notification.connection_id.0,
                "dropping mcp/message notification for unknown connection"
            );
            return;
        };
        let frame = serde_json::json!({
            "connectionId": notification.connection_id,
            "method": notification.method,
            "params": notification.params,
        });
        if serde_json::to_string(&frame).map_or(0, |text| text.len()) > MCP_OVER_ACP_MAX_FRAME_BYTES
        {
            tracing::warn!(
                connection_id = %notification.connection_id.0,
                "closing mcp-over-acp connection: notification exceeds the frame cap"
            );
            self.close_connection(&notification.connection_id);
            return;
        }
        let inner = serde_json::json!({
            "jsonrpc": "2.0",
            "method": notification.method,
            "params": notification.params,
        });
        match serde_json::from_value::<RxJsonRpcMessage<RoleServer>>(inner) {
            Ok(message) => {
                if connection.rx_tx.send(message).is_err() {
                    tracing::debug!("dropping mcp/message notification: connection closed");
                }
            }
            Err(error) => {
                tracing::warn!(?error, "dropping invalid inner MCP notification");
            }
        }
    }

    /// Handles `mcp/disconnect`: closes the logical connection and answers
    /// with an empty response.  Unknown connection ids fail closed with
    /// invalid params.
    pub(crate) fn handle_disconnect(
        &self,
        request: DisconnectMcpRequest,
        responder: Responder<DisconnectMcpResponse>,
    ) -> Result<(), RpcError> {
        if self
            .connections
            .lock()
            .expect("mcp connections poisoned")
            .remove(&request.connection_id)
            .is_none()
        {
            return responder.respond_with_error(RpcError::invalid_params().data(
                serde_json::json!({
                    "reason": "unknown MCP connection",
                    "connectionId": request.connection_id,
                }),
            ));
        }
        responder.respond(DisconnectMcpResponse::new())
    }

    /// Closes one logical connection (disconnect and fail-closed paths).
    fn close_connection(&self, connection_id: &McpConnectionId) {
        if let Some(connection) =
            self.connections.lock().expect("mcp connections poisoned").remove(connection_id)
        {
            connection.close();
        }
    }

    /// Closes every logical connection.  Called on turn cancel, session
    /// close, agent disconnect, and app shutdown; idempotent.
    pub(crate) fn close_all(&self) {
        let connections = {
            let mut guard = self.connections.lock().expect("mcp connections poisoned");
            guard.drain().map(|(_, connection)| connection).collect::<Vec<_>>()
        };
        for connection in connections {
            connection.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::JsonRpcMessage as ModelJsonRpcMessage;
    use serde_json::json;

    #[test]
    fn mode_display_is_deterministic() {
        assert_eq!(EeProxyMode::AcpNative.to_string(), "acp-native");
        assert_eq!(EeProxyMode::StdioFallback.to_string(), "stdio fallback");
        assert_eq!(EeProxyMode::Disabled.to_string(), "disabled");
    }

    #[test]
    fn frame_cap_matches_stdio_proxy_cap() {
        // The ACP-native path must be at least as strict as the stdio proxy
        // fallback (ee-cli `PROXY_MAX_FRAME_BYTES`).
        assert_eq!(MCP_OVER_ACP_MAX_FRAME_BYTES, 4 * 1024 * 1024);
    }

    /// Inner MCP wire messages round-trip through the same deserialization
    /// the transport bridge uses.
    #[test]
    fn inner_mcp_request_deserializes_into_rmcp_types() {
        let value = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": null,
        });
        let message: RxJsonRpcMessage<RoleServer> =
            serde_json::from_value(value).expect("tools/list parses");
        match message {
            ModelJsonRpcMessage::Request(request) => {
                assert_eq!(request.id, RequestId::Number(1));
            }
            other => panic!("expected request, got {other:?}"),
        }
    }
}
