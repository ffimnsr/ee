//! Proxy socket server and bridge dispatch.
use super::proxy_calls::ProxyCall;
use super::proxy_calls::ProxyErrorBody;
use super::proxy_calls::ProxyReply;
use super::proxy_calls::read_bounded_line;
use super::*;

pub(super) async fn serve_proxy_connection(
    mut stream: tokio::net::UnixStream,
    token: String,
    bridge_tx: std_mpsc::Sender<BridgeUiMessage>,
) {
    use tokio::io::AsyncWriteExt;

    struct ConnectionScopeGuard {
        scope: String,
        bridge_tx: std_mpsc::Sender<BridgeUiMessage>,
    }

    impl Drop for ConnectionScopeGuard {
        fn drop(&mut self) {
            let _ = self
                .bridge_tx
                .send(BridgeUiMessage::ProxyConnectionClosed { scope: self.scope.clone() });
        }
    }
    let (read_half, mut write_half) = stream.split();
    let mut reader = tokio::io::BufReader::new(read_half);
    let Ok(Some(first)) = read_bounded_line(&mut reader, PROXY_TOKEN_MAX_BYTES).await else {
        return;
    };
    if first != token {
        let _ = write_half.write_all(b"{\"error\":\"bad token\"}\n").await;
        return;
    }
    static NEXT_PROXY_SESSION: AtomicU64 = AtomicU64::new(0);
    let scope = format!("proxy-{}", NEXT_PROXY_SESSION.fetch_add(1, Ordering::Relaxed));
    let _connection_scope =
        ConnectionScopeGuard { scope: scope.clone(), bridge_tx: bridge_tx.clone() };
    loop {
        let Ok(Some(line)) = read_bounded_line(&mut reader, PROXY_MAX_FRAME_BYTES).await else {
            return;
        };
        if line.is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
            let _ = write_half.write_all(b"{\"error\":\"invalid frame\"}\n").await;
            return;
        };
        let Some(id) = request.get("id").cloned() else {
            continue; // notification frames are ignored
        };
        let params = request.get("params").cloned().unwrap_or_else(|| serde_json::json!({}));
        let reply = match serde_json::from_value::<ProxyCall>(params) {
            Ok(call) => {
                let cancellation = tokio_util::sync::CancellationToken::new();
                match start_proxy_call_to_bridge(call, &scope, cancellation.clone(), &bridge_tx) {
                    Ok(mut reply_rx) => {
                        tokio::select! {
                            biased;
                            // Socket EOF or a pipelined frame cancels the pending call.
                            // Dropping `reply_rx` lets the approval path fail closed.
                            _ = read_bounded_line(&mut reader, PROXY_MAX_FRAME_BYTES) => {
                                cancellation.cancel();
                                return;
                            }
                            result = &mut reply_rx => proxy_reply_from_bridge(result),
                        }
                    }
                    Err(reply) => reply,
                }
            }
            Err(_) => ProxyReply::Err {
                error: ProxyErrorBody {
                    message: String::from("invalid proxy call"),
                    denied: false,
                },
            },
        };
        let frame = serde_json::json!({ "id": id, "result": reply });
        let _ = write_half.write_all(frame.to_string().as_bytes()).await;
        let _ = write_half.write_all(b"\n").await;
        let _ = write_half.flush().await;
    }
}

pub(super) fn start_proxy_call_to_bridge(
    call: ProxyCall,
    scope: &str,
    cancellation: tokio_util::sync::CancellationToken,
    bridge_tx: &std_mpsc::Sender<BridgeUiMessage>,
) -> Result<oneshot::Receiver<ClientRequestResult>, ProxyReply> {
    let session_id = SessionId::new("proxy");
    let call = match call {
        ProxyCall::RememberWorkspaceFact { key, value } => {
            ProxyToolCall::RememberWorkspaceFact { key, value }
        }
        ProxyCall::RecallWorkspaceFacts { query } => ProxyToolCall::RecallWorkspaceFacts { query },
        ProxyCall::ReadWorkspaceFact { key } => ProxyToolCall::ReadWorkspaceFact { key },
        ProxyCall::ForgetWorkspaceFact { key } => ProxyToolCall::ForgetWorkspaceFact { key },
        ProxyCall::ListWorkspaceFacts { limit } => ProxyToolCall::ListWorkspaceFacts { limit },
        ProxyCall::RetractWorkspaceFact { key } => ProxyToolCall::RetractWorkspaceFact { key },
        ProxyCall::ExportWorkspaceMemory { include_values } => {
            ProxyToolCall::ExportWorkspaceMemory { include_values }
        }
        ProxyCall::ImportWorkspaceMemory { export_json } => {
            ProxyToolCall::ImportWorkspaceMemory { export_json }
        }
        ProxyCall::ClearWorkspaceMemory => ProxyToolCall::ClearWorkspaceMemory,
        ProxyCall::WorkspaceRoots => ProxyToolCall::WorkspaceRoots,
        ProxyCall::ListDirectory { path } => ProxyToolCall::ListDirectory { path },
        ProxyCall::ListDirectoryAll { path } => ProxyToolCall::ListDirectoryAll { path },
        ProxyCall::SearchFiles { pattern } => ProxyToolCall::SearchFiles { pattern },
        ProxyCall::SearchFilesAll { pattern } => ProxyToolCall::SearchFilesAll { pattern },
        ProxyCall::SearchText { query } => ProxyToolCall::SearchText { query },
        ProxyCall::SearchTextRegex { pattern } => ProxyToolCall::SearchTextRegex { pattern },
        ProxyCall::WebSearch { query } => {
            ProxyToolCall::WebSearch { query, approval_scope: scope.to_owned(), cancellation }
        }
        ProxyCall::FetchUrl { url } => {
            ProxyToolCall::FetchUrl { url, approval_scope: scope.to_owned(), cancellation }
        }
        ProxyCall::BrowserRun { request } => {
            ProxyToolCall::BrowserRun { request, approval_scope: scope.to_owned(), cancellation }
        }
        ProxyCall::SearchTextInFiles { query, file_glob } => {
            ProxyToolCall::SearchTextInFiles { query, file_glob }
        }
        ProxyCall::ReplaceText { path, old_text, new_text } => {
            ProxyToolCall::ReplaceText { path, old_text, new_text }
        }
        ProxyCall::ApplyPatch { path, edits } => ProxyToolCall::ApplyPatch {
            path,
            edits: edits
                .into_iter()
                .map(|edit| ee_agent_host::ProxyTextEdit {
                    old_text: edit.old_text,
                    new_text: edit.new_text,
                })
                .collect(),
        },
        ProxyCall::CreateTextFile { path, content } => {
            ProxyToolCall::CreateTextFile { path, content }
        }
        ProxyCall::OverwriteTextFile { path, content } => {
            ProxyToolCall::OverwriteTextFile { path, content }
        }
        ProxyCall::CreateDirectory { path } => ProxyToolCall::CreateDirectory { path },
        ProxyCall::DeletePath { path } => ProxyToolCall::DeletePath { path },
        ProxyCall::CopyPath { source_path, destination_path } => {
            ProxyToolCall::CopyPath { source_path, destination_path }
        }
        ProxyCall::MovePath { source_path, destination_path } => {
            ProxyToolCall::MovePath { source_path, destination_path }
        }
        ProxyCall::ReadBuffer { path } => ProxyToolCall::ReadBuffer { path },
        ProxyCall::ReadBufferLines { path, line, limit } => {
            ProxyToolCall::ReadBufferLines { path, line, limit }
        }
        ProxyCall::OpenBuffers => ProxyToolCall::OpenBuffers,
        ProxyCall::GetDiagnostics => ProxyToolCall::GetDiagnostics,
        ProxyCall::GetFileDiagnostics { path } => ProxyToolCall::GetFileDiagnostics { path },
        ProxyCall::DocumentSymbols { path } => ProxyToolCall::DocumentSymbols { path },
        ProxyCall::References { path, line, character } => {
            ProxyToolCall::References { path, line, character }
        }
        ProxyCall::ListCodeActions { path, line, character } => {
            ProxyToolCall::ListCodeActions { path, line, character }
        }
        ProxyCall::ApplyCodeAction { path, action_id } => {
            ProxyToolCall::ApplyCodeAction { path, action_id }
        }
        ProxyCall::FormatFile { path } => ProxyToolCall::FormatFile { path },
        ProxyCall::PreviewRenameSymbol { path, line, character, new_name } => {
            ProxyToolCall::PreviewRenameSymbol { path, line, character, new_name }
        }
        ProxyCall::RenameSymbol { path, line, character, new_name } => {
            ProxyToolCall::RenameSymbol { path, line, character, new_name }
        }
        ProxyCall::GitStatus => ProxyToolCall::GitStatus,
        ProxyCall::GitDiff => ProxyToolCall::GitDiff,
        ProxyCall::GitDiffStaged => ProxyToolCall::GitDiffStaged,
        ProxyCall::GitDiffFile { path } => ProxyToolCall::GitDiffFile { path },
        ProxyCall::ChangedFiles => ProxyToolCall::ChangedFiles,
        ProxyCall::ReviewContext => ProxyToolCall::ReviewContext,
        ProxyCall::ProjectInstructions => ProxyToolCall::ProjectInstructions,
        ProxyCall::SaveNote { key, content } => {
            ProxyToolCall::SaveNote { scope: scope.to_owned(), key, content }
        }
        ProxyCall::ReadNotes => ProxyToolCall::ReadNotes { scope: scope.to_owned() },
        ProxyCall::ReadNote { key } => ProxyToolCall::ReadNote { scope: scope.to_owned(), key },
        ProxyCall::FileDependencyMap { path } => ProxyToolCall::FileDependencyMap { path },
        ProxyCall::SymbolDependencyMap { path, line, character } => {
            ProxyToolCall::SymbolDependencyMap { path, line, character }
        }
        ProxyCall::ReadTextFile { path, line, limit } => {
            let mut request = ReadTextFileRequest::new(session_id, path);
            request.line = line;
            request.limit = limit;
            ProxyToolCall::Read(request)
        }
        ProxyCall::WriteTextFile { path, content } => {
            ProxyToolCall::Write(WriteTextFileRequest::new(session_id, path, content))
        }
        ProxyCall::TerminalCreate { command, args, cwd, env } => {
            let mut request = CreateTerminalRequest::new(session_id, command);
            request.args = args;
            if let Some(cwd) = cwd {
                request.cwd = Some(PathBuf::from(cwd));
            }
            request.env =
                env.into_iter().map(|(name, value)| EnvVariable::new(name, value)).collect();
            ProxyToolCall::Terminal(request)
        }
        ProxyCall::Diagnostics => ProxyToolCall::Diagnostics,
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    if bridge_tx
        .send(BridgeUiMessage::ProxyTool { call, route: ProxyRoute::Stdio, reply: reply_tx })
        .is_err()
    {
        return Err(ProxyReply::Err {
            error: ProxyErrorBody {
                message: String::from("editor is shutting down"),
                denied: false,
            },
        });
    }
    Ok(reply_rx)
}

pub(super) fn proxy_reply_from_bridge(
    result: Result<ClientRequestResult, oneshot::error::RecvError>,
) -> ProxyReply {
    match result {
        Ok(result) => ProxyReply::from_client_result(result),
        Err(_) => ProxyReply::Err {
            error: ProxyErrorBody {
                message: String::from("approval channel closed"),
                denied: false,
            },
        },
    }
}

/// Binds the proxy Unix socket and serves connections until shutdown.
pub(super) async fn serve_proxy_listener(
    info: ProxyInfo,
    bridge_tx: std_mpsc::Sender<BridgeUiMessage>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let listener = match tokio::net::UnixListener::bind(&info.socket_path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("ee: warning: mcp proxy listener bind failed: {error}");
            return;
        }
    };
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let token = info.token.clone();
                        let bridge = bridge_tx.clone();
                        connections.spawn(async move {
                            serve_proxy_connection(stream, token, bridge).await;
                        });
                    }
                    Err(error) => {
                        eprintln!("ee: warning: mcp proxy accept failed: {error}");
                    }
                }
            }
        }
    }
    connections.abort_all();
}
