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
    DisconnectMcpResponse, Error as RpcError, JsonRpcResponse, KillTerminalRequest,
    McpConnectionId, McpServerAcpId, MessageMcpNotification, MessageMcpRequest, MessageMcpResponse,
    ReleaseTerminalRequest, Responder, SessionId, TerminalId, TerminalOutputRequest,
    WaitForTerminalExitRequest,
};
use ee_mcp::{
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
use rmcp::model::{JsonRpcMessage, RequestId, ServerNotification, ServerRequest, ServerResult};
use rmcp::service::{RoleServer, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::error::AgentError;
use crate::inbound::{
    ClientRequest, ClientRequestHandler, ClientRequestResponse, HandlerCapabilities, ProxyTextEdit,
    WorkspaceMemoryMutationOperation,
};
use crate::process::AgentProcess;
use crate::session::ThreadShared;
use crate::turn_evidence::TurnKey;
use crate::workspace_memory::WorkspaceMemoryHost;
use crate::workspace_verified_facts::derive_workspace_verified_fact_candidates;

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
    threads: Arc<Mutex<HashMap<SessionId, Arc<ThreadShared>>>>,
    agent_id: String,
    scope: String,
    supported_tools: Option<Vec<String>>,
    workspace_memory: Arc<WorkspaceMemoryHost>,
    shutdown: CancellationToken,
}

impl HostProxyBackend {
    fn call_with_timeout(
        &self,
        request: ClientRequest,
        timeout: Duration,
    ) -> Result<Option<ClientRequestResponse>, ProxyToolError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let cancel = self.shutdown.child_token();
        if self.jobs.send(ProxyJob { request, cancel: cancel.clone(), reply: reply_tx }).is_err() {
            return Err(ProxyToolError {
                message: String::from("agent host is shutting down"),
                is_permission_denied: false,
            });
        }
        let started = std::time::Instant::now();
        loop {
            if self.shutdown.is_cancelled() {
                cancel.cancel();
                return Err(ProxyToolError {
                    message: "workspace_memory_approval_cancelled: operation cancelled".to_string(),
                    is_permission_denied: false,
                });
            }
            let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                cancel.cancel();
                return Ok(None);
            };
            let wait = remaining.min(Duration::from_millis(20));
            match reply_rx.recv_timeout(wait) {
                Ok(Ok(response)) => return Ok(Some(response)),
                Ok(Err(error)) => {
                    return Err(ProxyToolError {
                        message: error.to_string(),
                        is_permission_denied: matches!(error, AgentError::PermissionDenied { .. }),
                    });
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ProxyToolError {
                        message: String::from("agent host reply channel closed"),
                        is_permission_denied: false,
                    });
                }
            }
        }
    }

    fn call(&self, request: ClientRequest) -> Result<ClientRequestResponse, ProxyToolError> {
        self.call_with_timeout(request, MCP_OVER_ACP_APPROVAL_TIMEOUT)?.ok_or_else(|| {
            ProxyToolError {
                message: format!("approval timed out after {MCP_OVER_ACP_APPROVAL_TIMEOUT:?}"),
                is_permission_denied: false,
            }
        })
    }

    fn approve_workspace_memory_mutation(
        &self,
        operation: WorkspaceMemoryMutationOperation,
        key: String,
    ) -> Result<(), ProxyToolError> {
        match self.call(ClientRequest::ApproveWorkspaceMemoryMutation { operation, key })? {
            ClientRequestResponse::WorkspaceMemoryApproval { approved: true } => Ok(()),
            ClientRequestResponse::WorkspaceMemoryApproval { approved: false } => {
                Err(ProxyToolError {
                    message: "workspace_memory_approval_denied: workspace-memory mutation denied"
                        .to_string(),
                    is_permission_denied: true,
                })
            }
            _ => Err(ProxyToolError {
                message:
                    "workspace_memory_approval_invalid: invalid workspace-memory approval response"
                        .to_string(),
                is_permission_denied: false,
            }),
        }
    }

    fn memory_source_id(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"ee.workspace-memory.mcp-source.v1\0");
        digest.update(self.agent_id.as_bytes());
        digest.update(b"\0");
        digest.update(self.scope.as_bytes());
        format!("mcp:{:x}", digest.finalize())
    }

    fn evidence_unavailable(message: &'static str) -> ProxyToolError {
        ProxyToolError {
            message: format!("evidence_unavailable: {message}"),
            is_permission_denied: false,
        }
    }

    fn resolve_evidence_thread(
        &self,
        session_id: Option<String>,
        turn_id: Option<u64>,
    ) -> Result<(Arc<ThreadShared>, u64), ProxyToolError> {
        if turn_id.is_some() && session_id.is_none() {
            return Err(Self::evidence_unavailable(
                "a specified turn requires a connection-owned session",
            ));
        }

        let (thread, turn_id) = match session_id {
            Some(session_id) => {
                let session = SessionId::new(session_id);
                let thread = self
                    .threads
                    .lock()
                    .expect("threads poisoned")
                    .get(&session)
                    .cloned()
                    .ok_or_else(|| {
                        Self::evidence_unavailable("session is not owned by this connection")
                    })?;
                if thread.agent_id != self.agent_id || thread.session_id != session {
                    return Err(Self::evidence_unavailable("session ownership validation failed"));
                }
                let turn_id = match turn_id {
                    Some(turn_id) => turn_id,
                    None => thread
                        .active_turn
                        .lock()
                        .expect("active turn poisoned")
                        .as_ref()
                        .map(|turn| turn.turn_id())
                        .ok_or_else(|| {
                            Self::evidence_unavailable("session has no current evidence turn")
                        })?,
                };
                (thread, turn_id)
            }
            None => {
                let candidates = self
                    .threads
                    .lock()
                    .expect("threads poisoned")
                    .values()
                    .filter_map(|thread| {
                        if thread.agent_id != self.agent_id {
                            return None;
                        }
                        thread
                            .active_turn
                            .lock()
                            .expect("active turn poisoned")
                            .as_ref()
                            .map(|turn| (thread.clone(), turn.turn_id()))
                    })
                    .collect::<Vec<_>>();
                match candidates.as_slice() {
                    [(thread, turn_id)] => (thread.clone(), *turn_id),
                    [] => return Err(Self::evidence_unavailable("no current evidence turn")),
                    _ => {
                        return Err(Self::evidence_unavailable(
                            "current evidence turn is ambiguous; specify session_id",
                        ));
                    }
                }
            }
        };
        Ok((thread, turn_id))
    }
}

impl EeProxyBackend for HostProxyBackend {
    fn supported_tools(&self) -> Option<Vec<String>> {
        self.supported_tools.clone()
    }

    fn exposes_turn_evidence_summary(&self) -> bool {
        self.threads
            .lock()
            .expect("threads poisoned")
            .values()
            .any(|thread| thread.agent_id == self.agent_id && thread.has_turn_evidence())
    }

    fn web_search(&self, request: WebSearchRequest) -> Result<WebSearchResult, ProxyToolError> {
        proxy_value(
            self.call(ClientRequest::ProxyWebSearch {
                query: request.query,
                scope: self.scope.clone(),
            })?,
            "web_search",
        )
    }

    fn fetch_url(&self, request: FetchUrlRequest) -> Result<FetchUrlResult, ProxyToolError> {
        proxy_value(
            self.call(ClientRequest::ProxyFetchUrl {
                url: request.url,
                scope: self.scope.clone(),
            })?,
            "fetch_url",
        )
    }

    fn browser_run(&self, request: BrowserRunRequest) -> Result<BrowserRunResult, ProxyToolError> {
        proxy_value(
            self.call(ClientRequest::ProxyBrowserRun { request, scope: self.scope.clone() })?,
            "browser_run",
        )
    }

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

    fn create_directory(&self, path: String) -> Result<FilesystemResult, ProxyToolError> {
        proxy_value(self.call(ClientRequest::ProxyCreateDirectory { path })?, "create_directory")
    }

    fn delete_path(&self, path: String) -> Result<FilesystemResult, ProxyToolError> {
        proxy_value(self.call(ClientRequest::ProxyDeletePath { path })?, "delete_path")
    }

    fn copy_path(
        &self,
        source_path: String,
        destination_path: String,
    ) -> Result<FilesystemResult, ProxyToolError> {
        proxy_value(
            self.call(ClientRequest::ProxyCopyPath { source_path, destination_path })?,
            "copy_path",
        )
    }

    fn move_path(
        &self,
        source_path: String,
        destination_path: String,
    ) -> Result<FilesystemResult, ProxyToolError> {
        proxy_value(
            self.call(ClientRequest::ProxyMovePath { source_path, destination_path })?,
            "move_path",
        )
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

    fn git_status(&self) -> Result<GitStatusResult, ProxyToolError> {
        proxy_value(self.call(ClientRequest::ProxyGitStatus)?, "git_status")
    }

    fn git_diff(&self) -> Result<GitDiffResult, ProxyToolError> {
        proxy_value(self.call(ClientRequest::ProxyGitDiff)?, "git_diff")
    }

    fn git_diff_staged(&self) -> Result<GitDiffResult, ProxyToolError> {
        proxy_value(self.call(ClientRequest::ProxyGitDiffStaged)?, "git_diff_staged")
    }

    fn git_diff_file(&self, path: String) -> Result<GitDiffResult, ProxyToolError> {
        proxy_value(self.call(ClientRequest::ProxyGitDiffFile { path })?, "git_diff_file")
    }

    fn changed_files(&self) -> Result<ChangedFilesResult, ProxyToolError> {
        proxy_value(self.call(ClientRequest::ProxyChangedFiles)?, "changed_files")
    }

    fn review_context(&self) -> Result<ReviewContextResult, ProxyToolError> {
        proxy_value(self.call(ClientRequest::ProxyReviewContext)?, "review_context")
    }

    fn turn_evidence_summary(
        &self,
        session_id: Option<String>,
        turn_id: Option<u64>,
    ) -> Result<serde_json::Value, ProxyToolError> {
        let (thread, turn_id) = self.resolve_evidence_thread(session_id, turn_id)?;
        let summary = thread
            .evidence_summary(turn_id)
            .ok_or_else(|| Self::evidence_unavailable("turn evidence is missing or stale"))?;
        let expected_key =
            TurnKey::new(self.agent_id.clone(), thread.session_id.0.to_string(), turn_id);
        if summary.key != expected_key {
            return Err(Self::evidence_unavailable("turn ownership validation failed"));
        }
        serde_json::to_value(summary).map_err(|_| ProxyToolError {
            message: String::from("evidence_unavailable: turn summary serialization failed"),
            is_permission_denied: false,
        })
    }

    fn project_instructions(&self) -> Result<ProjectInstructionsResult, ProxyToolError> {
        proxy_value(self.call(ClientRequest::ProxyProjectInstructions)?, "project_instructions")
    }

    fn save_note(&self, key: String, content: String) -> Result<SessionNoteResult, ProxyToolError> {
        proxy_value(
            self.call(ClientRequest::ProxySaveNote { scope: self.scope.clone(), key, content })?,
            "save_note",
        )
    }

    fn read_notes(&self) -> Result<SessionNotesResult, ProxyToolError> {
        proxy_value(
            self.call(ClientRequest::ProxyReadNotes { scope: self.scope.clone() })?,
            "read_notes",
        )
    }

    fn read_note(&self, key: String) -> Result<SessionNoteResult, ProxyToolError> {
        proxy_value(
            self.call(ClientRequest::ProxyReadNote { scope: self.scope.clone(), key })?,
            "read_note",
        )
    }

    fn remember_workspace_fact(
        &self,
        key: String,
        value: String,
    ) -> Result<WorkspaceFactMutationResult, ProxyToolError> {
        self.approve_workspace_memory_mutation(
            WorkspaceMemoryMutationOperation::Remember,
            key.clone(),
        )?;
        self.workspace_memory.remember(key, value, self.memory_source_id())
    }

    fn verify_workspace_fact(
        &self,
        session_id: String,
        turn_id: u64,
        key: String,
    ) -> Result<WorkspaceFactMutationResult, ProxyToolError> {
        let (thread, resolved_turn_id) =
            self.resolve_evidence_thread(Some(session_id), Some(turn_id))?;
        if resolved_turn_id != turn_id {
            return Err(Self::evidence_unavailable("turn ownership validation failed"));
        }
        let evidence = thread
            .evidence_snapshot(turn_id)
            .ok_or_else(|| Self::evidence_unavailable("turn evidence is missing or stale"))?;
        let candidate = derive_workspace_verified_fact_candidates(&evidence)
            .map_err(|_| Self::evidence_unavailable("turn evidence is not fully verified"))?
            .into_iter()
            .find(|candidate| candidate.key == key)
            .ok_or_else(|| Self::evidence_unavailable("fact key is not derived from this turn"))?;
        self.approve_workspace_memory_mutation(WorkspaceMemoryMutationOperation::Verify, key)?;
        self.workspace_memory.promote_verified(candidate, &evidence)
    }

    fn recall_workspace_facts(
        &self,
        query: String,
    ) -> Result<WorkspaceFactsResult, ProxyToolError> {
        self.workspace_memory.recall(query)
    }

    fn read_workspace_fact(&self, key: String) -> Result<WorkspaceFact, ProxyToolError> {
        self.workspace_memory.read(key)
    }

    fn forget_workspace_fact(
        &self,
        key: String,
    ) -> Result<WorkspaceFactMutationResult, ProxyToolError> {
        self.approve_workspace_memory_mutation(
            WorkspaceMemoryMutationOperation::Forget,
            key.clone(),
        )?;
        self.workspace_memory.forget(key)
    }

    fn list_workspace_facts(&self, limit: u32) -> Result<WorkspaceFactsResult, ProxyToolError> {
        self.workspace_memory.list(limit as usize)
    }

    fn retract_workspace_fact(
        &self,
        key: String,
    ) -> Result<WorkspaceFactMutationResult, ProxyToolError> {
        self.approve_workspace_memory_mutation(
            WorkspaceMemoryMutationOperation::Forget,
            format!("retract:{key}"),
        )?;
        self.workspace_memory.retract(key)
    }

    fn export_workspace_memory(
        &self,
        include_values: bool,
    ) -> Result<serde_json::Value, ProxyToolError> {
        self.approve_workspace_memory_mutation(
            WorkspaceMemoryMutationOperation::Remember,
            format!("export:include_values={include_values}"),
        )?;
        self.workspace_memory.export(include_values)
    }

    fn import_workspace_memory(
        &self,
        export_json: String,
    ) -> Result<serde_json::Value, ProxyToolError> {
        let export = WorkspaceMemoryHost::decode_import(&export_json)?;
        let metadata = format!(
            "import:schema={}:facts={}:redacted={}",
            export.schema_version,
            export.facts.len(),
            export.redacted
        );
        self.approve_workspace_memory_mutation(
            WorkspaceMemoryMutationOperation::Remember,
            metadata,
        )?;
        self.workspace_memory.import(export)
    }

    fn clear_workspace_memory(&self) -> Result<serde_json::Value, ProxyToolError> {
        self.approve_workspace_memory_mutation(
            WorkspaceMemoryMutationOperation::Forget,
            "clear:workspace".to_string(),
        )?;
        self.workspace_memory.clear()
    }

    fn file_dependency_map(&self, path: String) -> Result<FileDependencyMapResult, ProxyToolError> {
        proxy_value(
            self.call(ClientRequest::ProxyFileDependencyMap { path })?,
            "file_dependency_map",
        )
    }

    fn symbol_dependency_map(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> Result<SymbolDependencyMapResult, ProxyToolError> {
        proxy_value(
            self.call(ClientRequest::ProxySymbolDependencyMap { path, line, character })?,
            "symbol_dependency_map",
        )
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

    fn terminal_output(&self, terminal_id: String) -> Result<TerminalOutputResult, ProxyToolError> {
        let request =
            TerminalOutputRequest::new(SessionId::new("proxy"), TerminalId::new(terminal_id));
        match self.call(ClientRequest::ProxyTerminalOutput(request))? {
            ClientRequestResponse::ProxyValue(value) => {
                serde_json::from_value(value).map_err(|error| ProxyToolError {
                    message: format!("proxy terminal output returned invalid payload: {error}"),
                    is_permission_denied: false,
                })
            }
            _ => Err(ProxyToolError {
                message: String::from("proxy terminal output returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn terminal_output_since(
        &self,
        terminal_id: String,
        since_seq: u64,
    ) -> Result<TerminalOutputResult, ProxyToolError> {
        let mut output = self.terminal_output(terminal_id)?;
        output.chunks.retain(|chunk| chunk.sequence > since_seq);
        output.output = output.chunks.iter().map(|chunk| chunk.text.as_str()).collect();
        Ok(output)
    }

    fn terminal_wait(&self, terminal_id: String) -> Result<TerminalWaitResult, ProxyToolError> {
        let request =
            WaitForTerminalExitRequest::new(SessionId::new("proxy"), TerminalId::new(terminal_id));
        match self.call_with_timeout(
            ClientRequest::WaitForTerminalExit(request),
            MCP_OVER_ACP_APPROVAL_TIMEOUT,
        )? {
            Some(ClientRequestResponse::WaitForTerminalExit(response)) => {
                let exit_status =
                    serde_json::to_value(response.exit_status).map_err(|error| ProxyToolError {
                        message: format!(
                            "proxy terminal wait returned an invalid exit status: {error}"
                        ),
                        is_permission_denied: false,
                    })?;
                Ok(TerminalWaitResult { completed: true, exit_status: Some(exit_status) })
            }
            Some(_) => Err(ProxyToolError {
                message: String::from("proxy terminal wait returned an unexpected response"),
                is_permission_denied: false,
            }),
            None => Ok(TerminalWaitResult { completed: false, exit_status: None }),
        }
    }

    fn terminal_wait_long(
        &self,
        terminal_id: String,
        timeout_ms: u64,
    ) -> Result<TerminalWaitResult, ProxyToolError> {
        let timeout = Duration::from_millis(timeout_ms.min(5 * 60 * 1_000));
        let request =
            WaitForTerminalExitRequest::new(SessionId::new("proxy"), TerminalId::new(terminal_id));
        match self.call_with_timeout(ClientRequest::WaitForTerminalExit(request), timeout)? {
            Some(ClientRequestResponse::WaitForTerminalExit(response)) => {
                let exit_status =
                    serde_json::to_value(response.exit_status).map_err(|error| ProxyToolError {
                        message: format!(
                            "proxy terminal wait returned an invalid exit status: {error}"
                        ),
                        is_permission_denied: false,
                    })?;
                Ok(TerminalWaitResult { completed: true, exit_status: Some(exit_status) })
            }
            Some(_) => Err(ProxyToolError {
                message: String::from("proxy terminal wait returned an unexpected response"),
                is_permission_denied: false,
            }),
            None => Ok(TerminalWaitResult { completed: false, exit_status: None }),
        }
    }

    fn terminal_kill(&self, terminal_id: String) -> Result<(), ProxyToolError> {
        let request =
            KillTerminalRequest::new(SessionId::new("proxy"), TerminalId::new(terminal_id));
        match self.call(ClientRequest::KillTerminal(request))? {
            ClientRequestResponse::KillTerminal(_) => Ok(()),
            _ => Err(ProxyToolError {
                message: String::from("proxy terminal kill returned an unexpected response"),
                is_permission_denied: false,
            }),
        }
    }

    fn terminal_release(&self, terminal_id: String) -> Result<(), ProxyToolError> {
        let request =
            ReleaseTerminalRequest::new(SessionId::new("proxy"), TerminalId::new(terminal_id));
        match self.call(ClientRequest::ReleaseTerminal(request))? {
            ClientRequestResponse::ReleaseTerminal(_) => Ok(()),
            _ => Err(ProxyToolError {
                message: String::from("proxy terminal release returned an unexpected response"),
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
    /// Active host session threads for this agent connection. MCP wire calls
    /// contain no session id, so evidence retrieval must resolve only through
    /// this connection-owned map.
    threads: Arc<Mutex<HashMap<SessionId, Arc<ThreadShared>>>>,
    agent_id: String,
    proxy_discovery: bool,
    workspace_memory: Arc<WorkspaceMemoryHost>,
    tool_profile: EeProxyToolProfile,
}

impl McpOverAcpRegistry {
    /// Creates the registry; `enabled` arms MCP-over-ACP for this
    /// connection.  The executor task is spawned on the current runtime and
    /// exits when the connection drops the jobs sender.
    pub(crate) fn new(
        enabled: bool,
        agent_id: &str,
        handler: Arc<dyn ClientRequestHandler>,
        process: Arc<Mutex<Option<AgentProcess>>>,
        threads: Arc<Mutex<HashMap<SessionId, Arc<ThreadShared>>>>,
        workspace_memory: Arc<WorkspaceMemoryHost>,
        tool_profile: EeProxyToolProfile,
    ) -> Self {
        let handler_capabilities = handler.capabilities();
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
            threads,
            agent_id: agent_id.to_owned(),
            proxy_discovery: handler_capabilities.proxy_discovery,
            workspace_memory,
            tool_profile,
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
        let backend = HostProxyBackend {
            jobs: self.jobs.clone(),
            process: self.process.clone(),
            threads: self.threads.clone(),
            agent_id: self.agent_id.clone(),
            scope: connection_id.to_string(),
            workspace_memory: self.workspace_memory.clone(),
            shutdown: shutdown.clone(),
            supported_tools: match self.tool_profile {
                EeProxyToolProfile::Full => (!self.proxy_discovery).then(Vec::new),
                EeProxyToolProfile::CriticReadOnly => Some(
                    ee_mcp::critic_read_only_tool_names(ee_mcp::ToolTransport::Acp)
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                ),
            },
        };
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
                // `serve_server_with_ct` resolves after first-request
                // `server/discover` negotiation and returns running service;
                // await it so transport and rx channel stay alive until
                // connection cancellation or closure.
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

    /// Handles an `mcp/message` notification (for example,
    /// `notifications/cancelled`). Unknown connections are dropped with a
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
        let connection = self
            .connections
            .lock()
            .expect("mcp connections poisoned")
            .remove(&request.connection_id);
        let Some(connection) = connection else {
            return responder.respond_with_error(RpcError::invalid_params().data(
                serde_json::json!({
                    "reason": "unknown MCP connection",
                    "connectionId": request.connection_id,
                }),
            ));
        };
        connection.close();
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
    use crate::reducer::SessionState;
    use crate::turn_evidence::{
        EvidenceCheck, EvidenceRevision, HostValidationRecord, PromptTerminalOutcome,
        TurnEvidenceStore, TurnObservation, WriteEvidenceOutcome,
    };
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

    #[test]
    fn proxy_terminal_output_uses_structured_proxy_response() {
        let (jobs, mut received) = mpsc::unbounded_channel();
        let backend = HostProxyBackend {
            jobs,
            process: Arc::new(Mutex::new(None)),
            threads: Arc::new(Mutex::new(HashMap::new())),
            agent_id: String::from("test-agent"),
            scope: String::from("test"),
            supported_tools: None,
            workspace_memory: WorkspaceMemoryHost::disabled(),
            shutdown: CancellationToken::new(),
        };
        let worker = std::thread::spawn(move || {
            let job = received.blocking_recv().expect("proxy terminal output request");
            match job.request {
                ClientRequest::ProxyTerminalOutput(request) => {
                    assert_eq!(request.session_id.0.as_ref(), "proxy");
                    assert_eq!(request.terminal_id.0.as_ref(), "term-1");
                }
                request => panic!("unexpected request: {request:?}"),
            }
            job.reply
                .send(Ok(ClientRequestResponse::ProxyValue(json!({
                    "output": "stdout",
                    "chunks": [],
                    "totalBytes": 6,
                    "truncated": false,
                    "exitStatus": null,
                    "running": true,
                    "elapsedMs": 1_000,
                }))))
                .expect("proxy terminal output response");
        });

        assert_eq!(
            backend.terminal_output(String::from("term-1")).expect("structured result"),
            TerminalOutputResult {
                output: String::from("stdout"),
                chunks: Vec::new(),
                total_bytes: 6,
                truncated: false,
                exit_status: None,
                running: true,
                elapsed_ms: 1_000,
            }
        );
        worker.join().expect("proxy terminal output worker");
    }

    #[test]
    fn proxy_terminal_output_since_filters_chunks_and_reconstructs_output() {
        let (jobs, mut received) = mpsc::unbounded_channel();
        let backend = HostProxyBackend {
            jobs,
            process: Arc::new(Mutex::new(None)),
            threads: Arc::new(Mutex::new(HashMap::new())),
            agent_id: String::from("test-agent"),
            scope: String::from("test"),
            supported_tools: None,
            workspace_memory: WorkspaceMemoryHost::disabled(),
            shutdown: CancellationToken::new(),
        };
        let worker = std::thread::spawn(move || {
            let job = received.blocking_recv().expect("proxy terminal output request");
            match job.request {
                ClientRequest::ProxyTerminalOutput(request) => {
                    assert_eq!(request.terminal_id.0.as_ref(), "term-1");
                }
                request => panic!("unexpected request: {request:?}"),
            }
            job.reply
                .send(Ok(ClientRequestResponse::ProxyValue(json!({
                    "output": "firstsecondthird",
                    "chunks": [
                        { "sequence": 1, "stream": "stdout", "text": "first" },
                        { "sequence": 2, "stream": "stderr", "text": "second" },
                        { "sequence": 3, "stream": "stdout", "text": "third" },
                    ],
                    "totalBytes": 16,
                    "truncated": false,
                    "exitStatus": null,
                }))))
                .expect("proxy terminal output response");
        });

        let output =
            backend.terminal_output_since(String::from("term-1"), 1).expect("filtered result");
        assert_eq!(output.output, "secondthird");
        assert_eq!(
            output.chunks.iter().map(|chunk| chunk.sequence).collect::<Vec<_>>(),
            vec![2, 3]
        );
        worker.join().expect("proxy terminal output worker");
    }

    #[test]
    fn turn_evidence_summary_returns_only_owned_redacted_host_summary() {
        let session = SessionId::new("session-1");
        let (events, _) = mpsc::unbounded_channel();
        let mut evidence = TurnEvidenceStore::default();
        let turn = evidence.start_turn(String::from("agent-1"), session.0.to_string());
        evidence
            .observe(
                turn.turn_id(),
                TurnObservation::ChangedFiles {
                    revision: EvidenceRevision::new("revision-1"),
                    files: vec![String::from("/private/workspace/secret.rs")],
                    truncated: false,
                },
            )
            .expect("host observation records");
        let shared = Arc::new(ThreadShared {
            agent_id: String::from("agent-1"),
            session_id: session.clone(),
            state: Mutex::new(SessionState::default()),
            order: Mutex::new(ee_agent_protocol::SessionUpdateOrder::new()),
            turn: Mutex::new(None),
            active_turn: Mutex::new(Some(turn.clone())),
            paused_turn: Mutex::new(None),
            turn_started: Mutex::new(None),
            evidence: Mutex::new(evidence),
            evidence_available: std::sync::atomic::AtomicBool::new(true),
            modes: Mutex::new(None),
            events,
        });
        let threads = Arc::new(Mutex::new(HashMap::from([(session.clone(), shared)])));
        let (jobs, _) = mpsc::unbounded_channel();
        let backend = HostProxyBackend {
            jobs,
            process: Arc::new(Mutex::new(None)),
            threads,
            agent_id: String::from("agent-1"),
            scope: String::from("test"),
            supported_tools: None,
            workspace_memory: WorkspaceMemoryHost::disabled(),
            shutdown: CancellationToken::new(),
        };

        assert!(backend.exposes_turn_evidence_summary());
        let current = backend.turn_evidence_summary(None, None).expect("current summary");
        assert!(
            current["key"]["agent_id"].as_str().is_some_and(|value| value.starts_with("sha256:"))
        );
        assert!(
            current["key"]["session_id"].as_str().is_some_and(|value| value.starts_with("sha256:"))
        );
        assert_eq!(current["key"]["turn_id"], 1);
        let serialized = current.to_string();
        assert!(!serialized.contains("agent-1"));
        assert!(!serialized.contains("session-1"));
        assert!(!serialized.contains("/private/workspace/secret.rs"));
        assert!(!serialized.contains("terminal output"));
        assert!(!serialized.contains("prompt"));

        assert!(backend.turn_evidence_summary(Some(String::from("session-1")), Some(1)).is_ok());
        for (session_id, turn_id) in [("foreign", 1), ("session-1", 2)] {
            let error = backend
                .turn_evidence_summary(Some(String::from(session_id)), Some(turn_id))
                .expect_err("foreign or stale evidence must fail closed");
            assert!(error.message.starts_with("evidence_unavailable:"));
        }
    }

    fn memory_backend() -> (HostProxyBackend, mpsc::UnboundedReceiver<ProxyJob>, tempfile::TempDir)
    {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("workspace");
        std::fs::create_dir(&root).expect("workspace root");
        let workspace_memory =
            WorkspaceMemoryHost::new(&crate::workspace_memory::WorkspaceMemoryHostConfig {
                enabled: true,
                trusted_roots: vec![root],
                database_path: Some(temp.path().join("memory.sqlite3")),
                ..Default::default()
            });
        let (jobs, received) = mpsc::unbounded_channel();
        (
            HostProxyBackend {
                jobs,
                process: Arc::new(Mutex::new(None)),
                threads: Arc::new(Mutex::new(HashMap::new())),
                agent_id: "agent-secret-name".to_string(),
                scope: "connection-secret-scope".to_string(),
                supported_tools: None,
                workspace_memory,
                shutdown: CancellationToken::new(),
            },
            received,
            temp,
        )
    }

    #[test]
    fn workspace_memory_mutations_require_typed_approval_without_value_disclosure() {
        let (backend, mut jobs, _temp) = memory_backend();
        let worker = std::thread::spawn(move || {
            for expected in [
                WorkspaceMemoryMutationOperation::Remember,
                WorkspaceMemoryMutationOperation::Forget,
            ] {
                let job = jobs.blocking_recv().expect("approval request");
                match job.request {
                    ClientRequest::ApproveWorkspaceMemoryMutation { operation, key } => {
                        assert_eq!(operation, expected);
                        assert_eq!(key, "architecture.parser");
                    }
                    request => panic!("unexpected request: {request:?}"),
                }
                job.reply
                    .send(Ok(ClientRequestResponse::WorkspaceMemoryApproval { approved: true }))
                    .expect("approval response");
            }
        });

        let remembered = backend
            .remember_workspace_fact(
                "architecture.parser".to_string(),
                "Tree-sitter remains backend-owned".to_string(),
            )
            .expect("approved remember");
        let fact = remembered.fact.expect("remembered fact");
        assert_eq!(fact.authority, "user_asserted");
        assert_eq!(fact.state, "active");
        assert_eq!(fact.provenance.source_kind, "mcp_user_approved");
        assert!(fact.provenance.source_id.starts_with("mcp:"));
        assert!(!fact.provenance.source_id.contains("agent-secret-name"));
        assert!(!fact.provenance.source_id.contains("connection-secret-scope"));

        // Reads bypass handler approval after opt-in.
        assert_eq!(
            backend
                .read_workspace_fact("architecture.parser".to_string())
                .expect("direct read")
                .value,
            "Tree-sitter remains backend-owned"
        );
        assert_eq!(
            backend
                .recall_workspace_facts("parser".to_string())
                .expect("direct recall")
                .facts
                .len(),
            1
        );
        assert_eq!(
            backend
                .forget_workspace_fact("architecture.parser".to_string())
                .expect("approved forget")
                .affected,
            1
        );
        worker.join().expect("approval worker");
    }

    #[test]
    fn workspace_memory_management_tools_use_bounded_metadata_without_values() {
        let (backend, mut jobs, _temp) = memory_backend();
        let secret_value = "Tree-sitter remains backend-owned";
        let worker = std::thread::spawn(move || {
            for (expected, metadata_prefix) in [
                (WorkspaceMemoryMutationOperation::Remember, "architecture.parser"),
                (WorkspaceMemoryMutationOperation::Remember, "export:include_values=true"),
                (WorkspaceMemoryMutationOperation::Forget, "retract:architecture.parser"),
                (WorkspaceMemoryMutationOperation::Remember, "import:schema="),
                (WorkspaceMemoryMutationOperation::Forget, "clear:workspace"),
            ] {
                let job = jobs.blocking_recv().expect("approval request");
                match job.request {
                    ClientRequest::ApproveWorkspaceMemoryMutation { operation, key } => {
                        assert_eq!(operation, expected);
                        assert!(key.starts_with(metadata_prefix), "{key}");
                        assert!(!key.contains(secret_value));
                    }
                    request => panic!("unexpected request: {request:?}"),
                }
                job.reply
                    .send(Ok(ClientRequestResponse::WorkspaceMemoryApproval { approved: true }))
                    .expect("approval response");
            }
        });

        backend
            .remember_workspace_fact("architecture.parser".to_string(), secret_value.to_string())
            .expect("remember");
        let listed = backend.list_workspace_facts(1).expect("list");
        assert_eq!(listed.facts.len(), 1);
        assert_eq!(listed.facts[0].value, secret_value);

        let export = backend.export_workspace_memory(true).expect("export");
        assert_eq!(export["redacted"], serde_json::json!(false));
        let export_json = serde_json::to_string(&export).expect("serialize export");
        backend.retract_workspace_fact("architecture.parser".to_string()).expect("retract");
        assert!(backend.read_workspace_fact("architecture.parser".to_string()).is_err());
        assert_eq!(
            backend.import_workspace_memory(export_json).expect("import")["affected"],
            serde_json::json!(1)
        );
        assert_eq!(backend.clear_workspace_memory().expect("clear")["affected"], json!(2));
        worker.join().expect("approval worker");
    }

    fn install_verified_turn(backend: &HostProxyBackend) -> String {
        let session = SessionId::new("verified-session");
        let revision = EvidenceRevision::new("revision-1");
        let mut evidence = TurnEvidenceStore::default();
        let turn = evidence.start_turn(backend.agent_id.clone(), session.0.to_string());
        for observation in [
            TurnObservation::Revision { revision: revision.clone() },
            TurnObservation::Write {
                revision: revision.clone(),
                outcome: WriteEvidenceOutcome::Applied,
            },
            TurnObservation::ChangedFiles {
                revision: revision.clone(),
                files: vec!["src/lib.rs".to_string()],
                truncated: false,
            },
            TurnObservation::Diagnostics {
                revision: revision.clone(),
                outcome: EvidenceCheck::Passed,
            },
            TurnObservation::DiffReview {
                revision: revision.clone(),
                outcome: EvidenceCheck::Passed,
            },
            TurnObservation::ValidationRecord {
                revision,
                selected: true,
                record: HostValidationRecord {
                    run_id: "validation-run".to_string(),
                    command_id: "cargo-test".to_string(),
                    command: "cargo test --quiet".to_string(),
                    tool: Some("terminal".to_string()),
                    selector: Some("cargo-test".to_string()),
                    outcome: EvidenceCheck::Passed,
                    exit_status: Some(0),
                    elapsed_ms: Some(10),
                    affected_tests: vec!["host".to_string()],
                    diagnostics_delta: 0,
                    output_truncated: false,
                    skip_or_denial: None,
                },
            },
            TurnObservation::PromptTerminal { outcome: PromptTerminalOutcome::Completed },
        ] {
            evidence.observe(turn.turn_id(), observation).expect("verified observation");
        }
        let snapshot = evidence.snapshot(turn.turn_id()).expect("verified evidence snapshot");
        let key = derive_workspace_verified_fact_candidates(&snapshot)
            .expect("verified candidate")
            .remove(0)
            .key;
        let (events, _) = mpsc::unbounded_channel();
        backend.threads.lock().expect("threads poisoned").insert(
            session.clone(),
            Arc::new(ThreadShared {
                agent_id: backend.agent_id.clone(),
                session_id: session,
                state: Mutex::new(SessionState::default()),
                order: Mutex::new(ee_agent_protocol::SessionUpdateOrder::new()),
                turn: Mutex::new(None),
                active_turn: Mutex::new(None),
                paused_turn: Mutex::new(None),
                turn_started: Mutex::new(None),
                evidence: Mutex::new(evidence),
                evidence_available: std::sync::atomic::AtomicBool::new(true),
                modes: Mutex::new(None),
                events,
            }),
        );
        key
    }

    #[test]
    fn verify_workspace_fact_promotes_only_exact_connection_owned_evidence() {
        let (backend, mut jobs, _temp) = memory_backend();
        let key = install_verified_turn(&backend);
        let approval_key = key.clone();
        let worker = std::thread::spawn(move || {
            let job = jobs.blocking_recv().expect("verify approval request");
            match job.request {
                ClientRequest::ApproveWorkspaceMemoryMutation { operation, key } => {
                    assert_eq!(operation, WorkspaceMemoryMutationOperation::Verify);
                    assert_eq!(key, approval_key);
                }
                request => panic!("unexpected request: {request:?}"),
            }
            job.reply
                .send(Ok(ClientRequestResponse::WorkspaceMemoryApproval { approved: true }))
                .expect("approval response");
        });

        let result = backend
            .verify_workspace_fact("verified-session".to_string(), 1, key.clone())
            .expect("verified fact promotion");
        let fact = result.fact.expect("promoted fact");
        assert_eq!(fact.key, key);
        assert_eq!(fact.authority, "host_verified");
        assert_eq!(fact.freshness, "revision_bound");
        assert_eq!(fact.state, "active");
        assert_eq!(fact.provenance.source_kind, "turn_evidence_validation");
        worker.join().expect("approval worker");
    }

    #[test]
    fn verify_workspace_fact_rejects_wrong_or_foreign_evidence_before_approval() {
        let (backend, mut jobs, _temp) = memory_backend();
        install_verified_turn(&backend);

        for (session, key) in [
            ("verified-session", "validation.wrong-key"),
            ("foreign-session", "validation.wrong-key"),
        ] {
            let error = backend
                .verify_workspace_fact(session.to_string(), 1, key.to_string())
                .expect_err("unowned or underived fact must fail closed");
            assert!(error.message.starts_with("evidence_unavailable:"));
        }
        assert!(matches!(jobs.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)));
    }

    #[test]
    fn workspace_memory_denial_prevents_mutation() {
        let (backend, mut jobs, _temp) = memory_backend();
        let worker = std::thread::spawn(move || {
            let job = jobs.blocking_recv().expect("approval request");
            job.reply
                .send(Ok(ClientRequestResponse::WorkspaceMemoryApproval { approved: false }))
                .expect("denial response");
        });
        let error = backend
            .remember_workspace_fact("denied.key".to_string(), "safe value".to_string())
            .expect_err("denial must fail");
        assert!(error.is_permission_denied);
        assert!(backend.read_workspace_fact("denied.key".to_string()).is_err());
        worker.join().expect("denial worker");
    }

    #[test]
    fn workspace_memory_approval_timeout_and_connection_cancel_abort_wait() {
        let (backend, mut jobs, _temp) = memory_backend();
        let timeout_worker = std::thread::spawn(move || {
            let job = jobs.blocking_recv().expect("timed request");
            while !job.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        assert!(
            backend
                .call_with_timeout(
                    ClientRequest::ApproveWorkspaceMemoryMutation {
                        operation: WorkspaceMemoryMutationOperation::Remember,
                        key: "timeout.key".to_string(),
                    },
                    Duration::from_millis(20),
                )
                .expect("timeout is typed")
                .is_none()
        );
        timeout_worker.join().expect("timeout worker");

        let (cancel_backend, _jobs, _temp) = memory_backend();
        let shutdown = cancel_backend.shutdown.clone();
        let cancel_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            shutdown.cancel();
        });
        let error = cancel_backend
            .call_with_timeout(
                ClientRequest::ApproveWorkspaceMemoryMutation {
                    operation: WorkspaceMemoryMutationOperation::Forget,
                    key: "cancel.key".to_string(),
                },
                Duration::from_secs(1),
            )
            .expect_err("shutdown cancels approval");
        assert!(error.message.starts_with("workspace_memory_approval_cancelled:"));
        cancel_thread.join().expect("cancel thread");
    }
}
