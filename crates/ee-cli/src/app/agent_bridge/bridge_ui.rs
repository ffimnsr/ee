//! ACP `ClientRequestHandler` bridge: forwards client requests into the app pump.

use std::pin::Pin;
use std::sync::mpsc as std_mpsc;

use ee_agent_host::{
    AgentError, ClientRequest, ClientRequestHandler, ClientRequestResponse, ClientRequestResult,
    HandlerCapabilities,
};
use ee_agent_protocol::{
    CreateElicitationRequest, CreateTerminalRequest, ElicitationScope, ReadTextFileRequest,
    SessionId, WriteTextFileRequest,
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::app::agents_mcp::ProxyRoute;

use super::terminal::{AgentTerminals, TerminalCompletion, TerminalOutputStream};

// ── Handler → pane messages ──────────────────────────────────────────────────

/// One agent-to-client request forwarded to the pane.
pub(crate) enum BridgeUiMessage {
    ReadFile {
        request: ReadTextFileRequest,
        reply: oneshot::Sender<ClientRequestResult>,
    },
    WriteFile {
        request: WriteTextFileRequest,
        reply: oneshot::Sender<ClientRequestResult>,
    },
    TerminalCreate {
        request: CreateTerminalRequest,
        reply: oneshot::Sender<ClientRequestResult>,
    },
    Elicitation {
        session_id: Option<SessionId>,
        request: CreateElicitationRequest,
        reply: oneshot::Sender<ClientRequestResult>,
    },
    WorkspaceMemoryApproval {
        operation: ee_agent_host::WorkspaceMemoryMutationOperation,
        key: String,
        reply: oneshot::Sender<ClientRequestResult>,
    },
    WorkspaceMemorySlashResult {
        text: String,
    },
    /// A tool call from the ee MCP proxy (Phase 6).  Writes and terminal
    /// creates queue the same approval prompts as direct ACP client methods;
    /// reads and diagnostics are served immediately.  `route` carries the
    /// transport that delivered the call (Phase 3 MCP trust).
    ProxyTool {
        call: crate::app::agents_mcp::ProxyToolCall,
        route: crate::app::agents_mcp::ProxyRoute,
        reply: oneshot::Sender<ClientRequestResult>,
    },
    /// Stdio proxy connection ended. Network grants and pending approvals are
    /// connection-scoped and must not survive a socket lifetime.
    ProxyConnectionClosed {
        scope: String,
    },
    /// Terminal lifecycle completion. Internal pane signal, not ACP or MCP.
    TerminalCompleted {
        completion: TerminalCompletion,
    },
}

async fn forward_and_await(
    tx: std_mpsc::Sender<BridgeUiMessage>,
    make: impl FnOnce(oneshot::Sender<ClientRequestResult>) -> BridgeUiMessage,
) -> ClientRequestResult {
    let (reply_tx, reply_rx) = oneshot::channel();
    if tx.send(make(reply_tx)).is_err() {
        return Err(AgentError::Cancelled);
    }
    match reply_rx.await {
        Ok(result) => result,
        Err(_) => Err(AgentError::Cancelled),
    }
}

/// Host handler: file requests and terminal creation are approved and
/// executed by the pane; terminal output/wait/kill/release run against the
/// shared registry on this (worker) thread.
pub(crate) struct BridgeUiHandler {
    tx: std_mpsc::Sender<BridgeUiMessage>,
    terminals: AgentTerminals,
}

impl BridgeUiHandler {
    #[must_use]
    pub(crate) fn new(tx: std_mpsc::Sender<BridgeUiMessage>, terminals: AgentTerminals) -> Self {
        Self { tx, terminals }
    }

    /// Exact editor-backed capability set wired by agents mode.
    ///
    /// Keep this explicit instead of using `HandlerCapabilities::all()` so new
    /// capability bits never become advertised in production before the editor
    /// bridge actually implements them.
    #[must_use]
    pub(crate) const fn editor_capabilities() -> HandlerCapabilities {
        HandlerCapabilities {
            fs_read: true,
            fs_write: true,
            terminal: true,
            elicitation_form: true,
            elicitation_url: true,
            session_config_boolean: true,
            proxy_discovery: true,
            workspace_memory_mutation_approval: true,
        }
    }
}

impl ClientRequestHandler for BridgeUiHandler {
    fn capabilities(&self) -> HandlerCapabilities {
        Self::editor_capabilities()
    }

    fn handle(
        &self,
        request: ClientRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = ClientRequestResult> + Send + '_>> {
        Box::pin(async move {
            match request {
                ClientRequest::ApproveWorkspaceMemoryMutation { operation, key } => {
                    forward_and_await(self.tx.clone(), |reply| {
                        BridgeUiMessage::WorkspaceMemoryApproval { operation, key, reply }
                    })
                    .await
                }
                ClientRequest::ProxyWorkspaceRoots => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::WorkspaceRoots,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyListDirectory { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::ListDirectory { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyListDirectoryAll { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::ListDirectoryAll { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySearchFiles { pattern } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::SearchFiles { pattern },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySearchFilesAll { pattern } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::SearchFilesAll { pattern },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySearchText { query } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::SearchText { query },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySearchTextRegex { pattern } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::SearchTextRegex { pattern },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyWebSearch { query, scope } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::WebSearch {
                            query,
                            approval_scope: scope,
                            cancellation: CancellationToken::new(),
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyFetchUrl { url, scope } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::FetchUrl {
                            url,
                            approval_scope: scope,
                            cancellation: CancellationToken::new(),
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyBrowserRun { request, scope } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::BrowserRun {
                            request,
                            approval_scope: scope,
                            cancellation: CancellationToken::new(),
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySearchTextInFiles { query, file_glob } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::SearchTextInFiles {
                            query,
                            file_glob,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyReplaceText { path, old_text, new_text } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::ReplaceText {
                            path,
                            old_text,
                            new_text,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyApplyPatch { path, edits } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::ApplyPatch { path, edits },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyCreateTextFile { path, content } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::CreateTextFile {
                            path,
                            content,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyOverwriteTextFile { path, content } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::OverwriteTextFile {
                            path,
                            content,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyCreateDirectory { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::CreateDirectory { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyDeletePath { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::DeletePath { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyCopyPath { source_path, destination_path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::CopyPath {
                            source_path,
                            destination_path,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyMovePath { source_path, destination_path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::MovePath {
                            source_path,
                            destination_path,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyReadBuffer { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::ReadBuffer { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyReadBufferLines { path, line, limit } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::ReadBufferLines {
                            path,
                            line,
                            limit,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyOpenBuffers => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::OpenBuffers,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyGetDiagnostics => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::GetDiagnostics,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyGetFileDiagnostics { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::GetFileDiagnostics { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyDocumentSymbols { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::DocumentSymbols { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyReferences { path, line, character } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::References {
                            path,
                            line,
                            character,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyListCodeActions { path, line, character } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::ListCodeActions {
                            path,
                            line,
                            character,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyApplyCodeAction { path, action_id } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::ApplyCodeAction {
                            path,
                            action_id,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyFormatFile { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::FormatFile { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyPreviewRenameSymbol { path, line, character, new_name } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::PreviewRenameSymbol {
                            path,
                            line,
                            character,
                            new_name,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyRenameSymbol { path, line, character, new_name } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::RenameSymbol {
                            path,
                            line,
                            character,
                            new_name,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyGitStatus => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::GitStatus,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyGitDiff => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::GitDiff,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyGitDiffStaged => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::GitDiffStaged,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyGitDiffFile { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::GitDiffFile { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyChangedFiles => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::ChangedFiles,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyReviewContext => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::ReviewContext,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyProjectInstructions => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::ProjectInstructions,
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySaveNote { scope, key, content } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::SaveNote {
                            scope,
                            key,
                            content,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyReadNotes { scope } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::ReadNotes { scope },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyReadNote { scope, key } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::ReadNote { scope, key },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxyFileDependencyMap { path } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::FileDependencyMap { path },
                        reply,
                    })
                    .await
                }
                ClientRequest::ProxySymbolDependencyMap { path, line, character } => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ProxyTool {
                        route: ProxyRoute::AcpNative,
                        call: crate::app::agents_mcp::ProxyToolCall::SymbolDependencyMap {
                            path,
                            line,
                            character,
                        },
                        reply,
                    })
                    .await
                }
                ClientRequest::ReadTextFile(request) => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::ReadFile {
                        request,
                        reply,
                    })
                    .await
                }
                ClientRequest::WriteTextFile(request) => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::WriteFile {
                        request,
                        reply,
                    })
                    .await
                }
                ClientRequest::CreateTerminal(request) => {
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::TerminalCreate {
                        request,
                        reply,
                    })
                    .await
                }
                ClientRequest::TerminalOutput(request) => {
                    self.terminals.output(&request).map(ClientRequestResponse::TerminalOutput)
                }
                ClientRequest::ProxyTerminalOutput(request) => {
                    self.terminals.output_snapshot(&request).and_then(|snapshot| {
                        let chunks = snapshot
                            .chunks
                            .into_iter()
                            .map(|chunk| ee_mcp::TerminalOutputChunk {
                                sequence: chunk.sequence,
                                stream: match chunk.stream {
                                    TerminalOutputStream::Stdout => String::from("stdout"),
                                    TerminalOutputStream::Stderr => String::from("stderr"),
                                },
                                text: chunk.text,
                            })
                            .collect();
                        let exit_status =
                            snapshot.exit_status.map(serde_json::to_value).transpose().map_err(
                                |error| {
                                    AgentError::HandlerError(format!(
                                        "terminal output exit status serialization failed: {error}"
                                    ))
                                },
                            )?;
                        Ok(ClientRequestResponse::ProxyValue(
                            serde_json::to_value(ee_mcp::TerminalOutputResult {
                                output: snapshot.combined_output,
                                chunks,
                                total_bytes: snapshot.total_bytes,
                                truncated: snapshot.truncated,
                                exit_status,
                                running: snapshot.running,
                                elapsed_ms: snapshot.elapsed_ms,
                            })
                            .map_err(|error| {
                                AgentError::HandlerError(format!(
                                    "terminal output serialization failed: {error}"
                                ))
                            })?,
                        ))
                    })
                }
                ClientRequest::WaitForTerminalExit(request) => {
                    let response = self.terminals.wait_for_exit(&request).await?;
                    if let Ok(completion) = self.terminals.completion(&request) {
                        let _ = self.tx.send(BridgeUiMessage::TerminalCompleted { completion });
                    }
                    Ok(ClientRequestResponse::WaitForTerminalExit(response))
                }
                ClientRequest::KillTerminal(request) => {
                    self.terminals.kill(&request).map(ClientRequestResponse::KillTerminal)
                }
                ClientRequest::ReleaseTerminal(request) => {
                    self.terminals.release(&request).map(ClientRequestResponse::ReleaseTerminal)
                }
                ClientRequest::CreateElicitation(request) => {
                    let session_id = match request.scope() {
                        ElicitationScope::Session(scope) => Some(scope.session_id.clone()),
                        _ => None,
                    };
                    forward_and_await(self.tx.clone(), |reply| BridgeUiMessage::Elicitation {
                        session_id,
                        request,
                        reply,
                    })
                    .await
                }
            }
        })
    }
}
