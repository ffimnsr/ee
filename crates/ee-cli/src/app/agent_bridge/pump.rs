//! `impl App`: bridge request pump, workspace-memory operations, proxy dispatch.

use std::path::{Path, PathBuf};

use ee_agent_host::{AgentError, ClientRequestResponse, ClientRequestResult};
use ee_agent_protocol::{SessionId, TerminalOutputResponse};
use tokio::sync::oneshot;

use super::super::*;

use crate::policy::TrustOutcome;

use super::approval::{
    WebApprovalCall, WorkspaceMemoryApprovalOperation, WorkspaceMemoryApprovalTarget,
    WriteExpectation,
};
use super::bridge_ui::BridgeUiMessage;
use super::prompt::ApprovalPrompt;

impl App {
    /// Drains bridge requests forwarded by the host handler.
    pub(crate) fn pump_bridge_requests(&mut self) {
        self.prune_cancelled_bridge_approvals();
        while let Ok(message) = self.agents.bridge_rx.try_recv() {
            match message {
                BridgeUiMessage::ReadFile { request, reply } => {
                    // Phase 4: normalize + evaluate before serving; reads
                    // stay prompt-free, but protected/external reads can
                    // never match a persistent rule.
                    let _ = self.native_read_decision(&request.path, request.limit.map(u64::from));
                    self.bridge_read_file(&request, reply);
                }
                BridgeUiMessage::WriteFile { request, reply } => {
                    if let Err(error) = self.validate_workspace_write_path(&request.path) {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                    let thread = self.session_thread(&request.session_id);
                    let persistent_label = self.native_write_persistent_label(
                        &request.path,
                        &request.content,
                        &WriteExpectation::Blind,
                    );
                    self.request_bridge_approval(ApprovalPrompt::write(
                        thread,
                        &request.session_id,
                        &request,
                        persistent_label,
                        reply,
                    ));
                }
                BridgeUiMessage::TerminalCreate { request, reply } => {
                    let thread = self.session_thread(&request.session_id);
                    let agent_id = thread
                        .and_then(|index| self.agents.threads.get(index))
                        .map(|thread| thread.agent_id.clone());
                    // Normalize after request validation and before approval
                    // queue insertion: only validated invocations may offer
                    // persistent command trust (Phase 2).
                    let persistent_allowed = self.command_invocation_for_request(&request).is_ok();
                    self.request_bridge_approval(ApprovalPrompt::terminal(
                        thread,
                        agent_id,
                        &request.session_id,
                        &request,
                        reply,
                        persistent_allowed,
                    ));
                }
                BridgeUiMessage::Elicitation { session_id, request, reply } => {
                    self.present_elicitation(session_id, request, reply);
                }
                BridgeUiMessage::WorkspaceMemoryApproval { operation, key, reply } => {
                    self.request_workspace_memory_approval(ApprovalPrompt::workspace_memory(
                        operation,
                        key,
                        WorkspaceMemoryApprovalTarget::ApprovalOnly,
                        reply,
                    ));
                }
                BridgeUiMessage::WorkspaceMemorySlashResult { text } => {
                    if let Some(index) = self.agents.active_thread_index() {
                        self.agents.threads[index].push_system(text);
                    } else {
                        self.backend.status_message = Some(text);
                    }
                }
                BridgeUiMessage::ProxyTool { call, route, reply } => {
                    self.handle_proxy_tool(call, route, reply);
                }
                BridgeUiMessage::ProxyConnectionClosed { scope } => {
                    self.clear_proxy_network_scope(&scope);
                }
                BridgeUiMessage::TerminalCompleted { completion } => {
                    self.record_terminal_validation(completion);
                }
            }
        }
    }

    fn workspace_memory_value<T: serde::Serialize>(
        result: Result<T, ee_agent_host::WorkspaceMemoryHostError>,
    ) -> Result<serde_json::Value, AgentError> {
        result.map_err(|error| AgentError::HandlerError(error.to_string())).and_then(|value| {
            serde_json::to_value(value).map_err(|error| AgentError::HandlerError(error.to_string()))
        })
    }

    pub(super) fn workspace_memory_remember(
        &self,
        key: &str,
        value: &str,
    ) -> Result<serde_json::Value, AgentError> {
        let manager = self
            .agents
            .host
            .as_ref()
            .ok_or_else(|| AgentError::HandlerError(String::from("agent host unavailable")))?;
        Self::workspace_memory_value(manager.manager.workspace_memory_remember_approved(
            key,
            value,
            ee_agent_host::WorkspaceMemoryMutationApproval::Approved,
        ))
    }

    pub(super) fn workspace_memory_forget(
        &self,
        key: &str,
    ) -> Result<serde_json::Value, AgentError> {
        let manager = self
            .agents
            .host
            .as_ref()
            .ok_or_else(|| AgentError::HandlerError(String::from("agent host unavailable")))?;
        Self::workspace_memory_value(manager.manager.workspace_memory_forget_approved(
            key,
            ee_agent_host::WorkspaceMemoryMutationApproval::Approved,
        ))
    }

    pub(super) fn workspace_memory_retract(
        &self,
        key: &str,
    ) -> Result<serde_json::Value, AgentError> {
        let manager = self
            .agents
            .host
            .as_ref()
            .ok_or_else(|| AgentError::HandlerError(String::from("agent host unavailable")))?;
        Self::workspace_memory_value(manager.manager.workspace_memory_retract_approved(
            key,
            ee_agent_host::WorkspaceMemoryMutationApproval::Approved,
        ))
    }

    pub(super) fn workspace_memory_clear(&self) -> Result<serde_json::Value, AgentError> {
        let manager = self
            .agents
            .host
            .as_ref()
            .ok_or_else(|| AgentError::HandlerError(String::from("agent host unavailable")))?;
        Self::workspace_memory_value(manager.manager.workspace_memory_clear_approved(
            ee_agent_host::WorkspaceMemoryMutationApproval::Approved,
        ))
    }

    pub(super) fn workspace_memory_disable_delete(
        &mut self,
        config_path: &Path,
    ) -> Result<serde_json::Value, AgentError> {
        let cleared = self.workspace_memory_clear()?;
        crate::config::persist_workspace_memory_enabled(config_path, false)
            .map_err(AgentError::HandlerError)?;
        self.config.agents.workspace_memory.enabled = false;
        Ok(serde_json::json!({
            "enabled": false,
            "cleared": cleared,
            "restart_required": true,
        }))
    }

    pub(super) fn workspace_memory_export_value(
        &self,
        include_values: bool,
    ) -> Result<serde_json::Value, AgentError> {
        let manager = self
            .agents
            .host
            .as_ref()
            .ok_or_else(|| AgentError::HandlerError(String::from("agent host unavailable")))?;
        Self::workspace_memory_value(manager.manager.workspace_memory_export_approved(
            include_values,
            ee_agent_host::WorkspaceMemoryMutationApproval::Approved,
        ))
    }

    pub(super) fn workspace_memory_export(
        &self,
        include_values: bool,
    ) -> Result<serde_json::Value, AgentError> {
        let manager = self
            .agents
            .host
            .as_ref()
            .ok_or_else(|| AgentError::HandlerError(String::from("agent host unavailable")))?;
        let export = manager
            .manager
            .workspace_memory_export_approved(
                include_values,
                ee_agent_host::WorkspaceMemoryMutationApproval::Approved,
            )
            .map_err(|error| AgentError::HandlerError(error.to_string()))?;
        #[cfg(test)]
        let directory =
            self.agents.test_export_base.as_ref().map(|base| base.join("workspace-memory-exports"));
        #[cfg(not(test))]
        let directory: Option<PathBuf> = None;
        let directory = match directory {
            Some(directory) => directory,
            None => crate::logs::state_dir()
                .map(|base| base.join("workspace-memory-exports"))
                .ok_or_else(|| {
                    AgentError::Io(String::from("platform state directory is unavailable"))
                })?,
        };
        let fact_count = export.facts.len();
        let redacted = export.redacted;
        let path = crate::app::agent_export::write_workspace_memory_export(&directory, &export)
            .map_err(|error| AgentError::Io(error.to_string()))?;
        Ok(serde_json::json!({
            "path": path,
            "redacted": redacted,
            "facts": fact_count,
        }))
    }

    pub(super) fn workspace_memory_import(
        &self,
        export: ee_agent_host::WorkspaceMemoryExportDto,
    ) -> Result<serde_json::Value, AgentError> {
        let manager = self
            .agents
            .host
            .as_ref()
            .ok_or_else(|| AgentError::HandlerError(String::from("agent host unavailable")))?;
        Self::workspace_memory_value(manager.manager.workspace_memory_import_approved(
            export,
            ee_agent_host::WorkspaceMemoryMutationApproval::Approved,
        ))
    }

    /// Answers one proxy tool call through the same approval/bridge paths as
    /// direct ACP client methods (Phase 6).  `fs/read_text_file` is served
    /// directly; writes and terminal creates queue an approval prompt;
    /// diagnostics return the last stderr text.
    fn handle_proxy_tool(
        &mut self,
        call: crate::app::agents_mcp::ProxyToolCall,
        route: crate::app::agents_mcp::ProxyRoute,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let session_id = SessionId::new("proxy");
        match call {
            crate::app::agents_mcp::ProxyToolCall::RememberWorkspaceFact { key, value } => {
                self.request_workspace_memory_approval(ApprovalPrompt::workspace_memory(
                    ee_agent_host::WorkspaceMemoryMutationOperation::Remember,
                    key,
                    WorkspaceMemoryApprovalTarget::Remember { value },
                    reply,
                ));
            }
            crate::app::agents_mcp::ProxyToolCall::RecallWorkspaceFacts { query } => {
                let result = self
                    .agents
                    .host
                    .as_ref()
                    .ok_or_else(|| AgentError::HandlerError(String::from("agent host unavailable")))
                    .and_then(|host| {
                        Self::workspace_memory_value(host.manager.workspace_memory_recall(
                            query,
                            self.config.agents.workspace_memory.max_recall_results,
                        ))
                    });
                let _ = reply.send(result.map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::ReadWorkspaceFact { key } => {
                let result = self
                    .agents
                    .host
                    .as_ref()
                    .ok_or_else(|| AgentError::HandlerError(String::from("agent host unavailable")))
                    .and_then(|host| {
                        Self::workspace_memory_value(host.manager.workspace_memory_read(key))
                    });
                let _ = reply.send(result.map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::ForgetWorkspaceFact { key } => {
                self.request_workspace_memory_approval(ApprovalPrompt::workspace_memory(
                    ee_agent_host::WorkspaceMemoryMutationOperation::Forget,
                    key,
                    WorkspaceMemoryApprovalTarget::Forget,
                    reply,
                ));
            }
            crate::app::agents_mcp::ProxyToolCall::ListWorkspaceFacts { limit } => {
                let result = self
                    .agents
                    .host
                    .as_ref()
                    .ok_or_else(|| AgentError::HandlerError(String::from("agent host unavailable")))
                    .and_then(|host| {
                        Self::workspace_memory_value(
                            host.manager.workspace_memory_list(limit as usize),
                        )
                    });
                let _ = reply.send(result.map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::RetractWorkspaceFact { key } => {
                self.request_workspace_memory_approval(ApprovalPrompt::workspace_memory_proxy(
                    WorkspaceMemoryApprovalOperation::Retract,
                    WorkspaceMemoryApprovalTarget::RetractKey { key },
                    reply,
                ));
            }
            crate::app::agents_mcp::ProxyToolCall::ExportWorkspaceMemory { include_values } => {
                self.request_workspace_memory_approval(ApprovalPrompt::workspace_memory_proxy(
                    WorkspaceMemoryApprovalOperation::Export,
                    WorkspaceMemoryApprovalTarget::ExportValue { include_values },
                    reply,
                ));
            }
            crate::app::agents_mcp::ProxyToolCall::ImportWorkspaceMemory { export_json } => {
                let export = match serde_json::from_str::<ee_agent_host::WorkspaceMemoryExportDto>(
                    &export_json,
                ) {
                    Ok(export) => export,
                    Err(_) => {
                        let _ = reply.send(Err(AgentError::HandlerError(String::from(
                            "workspace-memory import payload is invalid",
                        ))));
                        return;
                    }
                };
                self.request_workspace_memory_approval(ApprovalPrompt::workspace_memory_proxy(
                    WorkspaceMemoryApprovalOperation::Import,
                    WorkspaceMemoryApprovalTarget::Import { export: Box::new(export) },
                    reply,
                ));
            }
            crate::app::agents_mcp::ProxyToolCall::ClearWorkspaceMemory => {
                self.request_workspace_memory_approval(ApprovalPrompt::workspace_memory_proxy(
                    WorkspaceMemoryApprovalOperation::Clear,
                    WorkspaceMemoryApprovalTarget::Clear,
                    reply,
                ));
            }
            crate::app::agents_mcp::ProxyToolCall::WorkspaceRoots => {
                let _ =
                    reply.send(self.proxy_workspace_roots().map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::ListDirectory { path } => {
                let _ = reply.send(
                    self.proxy_list_directory(Path::new(&path), false)
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            crate::app::agents_mcp::ProxyToolCall::ListDirectoryAll { path } => {
                let _ = reply.send(
                    self.proxy_list_directory(Path::new(&path), true)
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            crate::app::agents_mcp::ProxyToolCall::SearchFiles { pattern } => {
                let _ = reply.send(
                    self.proxy_search_files(&pattern, false).map(ClientRequestResponse::ProxyValue),
                );
            }
            crate::app::agents_mcp::ProxyToolCall::SearchFilesAll { pattern } => {
                let _ = reply.send(
                    self.proxy_search_files(&pattern, true).map(ClientRequestResponse::ProxyValue),
                );
            }
            crate::app::agents_mcp::ProxyToolCall::SearchText { query } => {
                let _ = reply
                    .send(self.proxy_search_text(&query).map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::SearchTextRegex { pattern } => {
                let _ = reply.send(
                    self.proxy_search_text_regex(&pattern).map(ClientRequestResponse::ProxyValue),
                );
            }
            crate::app::agents_mcp::ProxyToolCall::WebSearch {
                query,
                approval_scope,
                cancellation,
            } => {
                self.queue_web_approval(
                    route,
                    approval_scope,
                    WebApprovalCall::Search { query },
                    cancellation,
                    reply,
                );
            }
            crate::app::agents_mcp::ProxyToolCall::FetchUrl {
                url,
                approval_scope,
                cancellation,
            } => {
                self.queue_web_approval(
                    route,
                    approval_scope,
                    WebApprovalCall::Fetch { url },
                    cancellation,
                    reply,
                );
            }
            crate::app::agents_mcp::ProxyToolCall::BrowserRun {
                request,
                approval_scope,
                cancellation,
            } => {
                self.queue_web_approval(
                    route,
                    approval_scope,
                    WebApprovalCall::BrowserRun { request },
                    cancellation,
                    reply,
                );
            }
            crate::app::agents_mcp::ProxyToolCall::SearchTextInFiles { query, file_glob } => {
                let _ = reply.send(
                    self.proxy_search_text_in_files(&query, &file_glob)
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            crate::app::agents_mcp::ProxyToolCall::ReplaceText { path, old_text, new_text } => {
                self.queue_proxy_replace_text(&path, &old_text, &new_text, reply);
            }
            crate::app::agents_mcp::ProxyToolCall::ApplyPatch { path, edits } => {
                self.queue_proxy_apply_patch(&path, &edits, reply);
            }
            crate::app::agents_mcp::ProxyToolCall::CreateTextFile { path, content } => {
                self.queue_proxy_create_text_file(&path, &content, reply);
            }
            crate::app::agents_mcp::ProxyToolCall::OverwriteTextFile { path, content } => {
                self.queue_proxy_overwrite_text_file(&path, &content, reply);
            }
            crate::app::agents_mcp::ProxyToolCall::CreateDirectory { path } => {
                self.queue_proxy_filesystem(
                    crate::app::agent_filesystem::FilesystemOperation::CreateDirectory {
                        path: PathBuf::from(path),
                    },
                    reply,
                );
            }
            crate::app::agents_mcp::ProxyToolCall::DeletePath { path } => {
                self.queue_proxy_filesystem(
                    crate::app::agent_filesystem::FilesystemOperation::DeletePath {
                        path: PathBuf::from(path),
                    },
                    reply,
                );
            }
            crate::app::agents_mcp::ProxyToolCall::CopyPath { source_path, destination_path } => {
                self.queue_proxy_filesystem(
                    crate::app::agent_filesystem::FilesystemOperation::CopyPath {
                        source: PathBuf::from(source_path),
                        destination: PathBuf::from(destination_path),
                    },
                    reply,
                );
            }
            crate::app::agents_mcp::ProxyToolCall::MovePath { source_path, destination_path } => {
                self.queue_proxy_filesystem(
                    crate::app::agent_filesystem::FilesystemOperation::MovePath {
                        source: PathBuf::from(source_path),
                        destination: PathBuf::from(destination_path),
                    },
                    reply,
                );
            }
            crate::app::agents_mcp::ProxyToolCall::ReadBuffer { path } => {
                let _ = reply.send(
                    self.proxy_read_buffer(Path::new(&path), None, None)
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            crate::app::agents_mcp::ProxyToolCall::ReadBufferLines { path, line, limit } => {
                let _ = reply.send(
                    self.proxy_read_buffer(Path::new(&path), Some(line), Some(limit))
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            crate::app::agents_mcp::ProxyToolCall::OpenBuffers => {
                let _ =
                    reply.send(self.proxy_open_buffers().map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::GetDiagnostics => {
                let _ = reply
                    .send(self.proxy_get_diagnostics(None).map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::GetFileDiagnostics { path } => {
                let _ = reply.send(
                    self.proxy_get_diagnostics(Some(Path::new(&path)))
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            crate::app::agents_mcp::ProxyToolCall::DocumentSymbols { path } => {
                let _ = reply.send(
                    self.proxy_document_symbols(Path::new(&path))
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            crate::app::agents_mcp::ProxyToolCall::References { path, line, character } => {
                let _ = reply.send(
                    self.proxy_references(Path::new(&path), line, character)
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            crate::app::agents_mcp::ProxyToolCall::ListCodeActions { path, line, character } => {
                let _ = reply.send(
                    self.proxy_list_code_actions(Path::new(&path), line, character)
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            crate::app::agents_mcp::ProxyToolCall::ApplyCodeAction { path, action_id } => {
                self.queue_proxy_apply_code_action(&path, &action_id, route, reply);
            }
            crate::app::agents_mcp::ProxyToolCall::FormatFile { path } => {
                self.queue_proxy_format_file(&path, route, reply);
            }
            crate::app::agents_mcp::ProxyToolCall::PreviewRenameSymbol {
                path,
                line,
                character,
                new_name,
            } => {
                let _ = reply.send(
                    self.proxy_preview_rename_symbol(Path::new(&path), line, character, &new_name)
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            crate::app::agents_mcp::ProxyToolCall::RenameSymbol {
                path,
                line,
                character,
                new_name,
            } => {
                self.queue_proxy_rename_symbol(&path, line, character, &new_name, route, reply);
            }
            crate::app::agents_mcp::ProxyToolCall::GitStatus => {
                let _ = reply.send(self.proxy_git_status().map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::GitDiff => {
                let _ = reply.send(self.proxy_git_diff().map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::GitDiffStaged => {
                let _ =
                    reply.send(self.proxy_git_diff_staged().map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::GitDiffFile { path } => {
                let _ = reply.send(
                    self.proxy_git_diff_file(Path::new(&path))
                        .map(ClientRequestResponse::ProxyValue),
                );
            }
            crate::app::agents_mcp::ProxyToolCall::ChangedFiles => {
                let _ =
                    reply.send(self.proxy_changed_files().map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::ReviewContext => {
                let _ =
                    reply.send(self.proxy_review_context().map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::ProjectInstructions => {
                let result = self
                    .active_root_path()
                    .ok_or_else(|| AgentError::invalid_params("no active workspace root"))
                    .and_then(|root| crate::app::agent_knowledge::project_instructions(&root))
                    .and_then(|result| {
                        serde_json::to_value(result)
                            .map_err(|error| AgentError::HandlerError(error.to_string()))
                    });
                let _ = reply.send(result.map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::SaveNote { scope, key, content } => {
                let result =
                    self.project_knowledge.save_note(&scope, &key, &content).and_then(|result| {
                        serde_json::to_value(result)
                            .map_err(|error| AgentError::HandlerError(error.to_string()))
                    });
                let _ = reply.send(result.map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::ReadNotes { scope } => {
                let result = self.project_knowledge.read_notes(&scope).and_then(|result| {
                    serde_json::to_value(result)
                        .map_err(|error| AgentError::HandlerError(error.to_string()))
                });
                let _ = reply.send(result.map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::ReadNote { scope, key } => {
                let result = self.project_knowledge.read_note(&scope, &key).and_then(|result| {
                    serde_json::to_value(result)
                        .map_err(|error| AgentError::HandlerError(error.to_string()))
                });
                let _ = reply.send(result.map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::FileDependencyMap { path } => {
                let result = self.proxy_file_dependency_map(Path::new(&path));
                let _ = reply.send(result.map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::SymbolDependencyMap {
                path,
                line,
                character,
            } => {
                let result = self.proxy_symbol_dependency_map(Path::new(&path), line, character);
                let _ = reply.send(result.map(ClientRequestResponse::ProxyValue));
            }
            crate::app::agents_mcp::ProxyToolCall::Read(request) => {
                // Existing read-only MCP behavior remains prompt-free until
                // this workspace explicitly enables the broad safe-read
                // profile. Once present, require its exact route/tool/schema
                // authorization before serving bytes.
                let decision = self.mcp_read_decision(&request, route);
                if self.mcp_read_profile_enforced() && decision.outcome != TrustOutcome::Allow {
                    let _ = reply.send(Err(AgentError::PermissionDenied {
                        reason: "MCP safe-read profile does not authorize this request".to_string(),
                    }));
                    return;
                }
                self.bridge_read_file(&request, reply);
            }
            crate::app::agents_mcp::ProxyToolCall::Write(request) => {
                if let Err(error) = self.validate_workspace_write_path(&request.path) {
                    let _ = reply.send(Err(error));
                    return;
                }
                self.request_bridge_approval(ApprovalPrompt::write(
                    None,
                    &session_id,
                    &request,
                    self.native_write_persistent_label(
                        &request.path,
                        &request.content,
                        &WriteExpectation::Blind,
                    ),
                    reply,
                ));
            }
            crate::app::agents_mcp::ProxyToolCall::Terminal(request) => {
                let persistent_allowed = self.command_invocation_for_request(&request).is_ok();
                self.request_bridge_approval(ApprovalPrompt::terminal(
                    None,
                    None,
                    &session_id,
                    &request,
                    reply,
                    persistent_allowed,
                ));
            }
            crate::app::agents_mcp::ProxyToolCall::Diagnostics => {
                // Transport-only mapping: diagnostics travel as terminal
                // output text internally and are re-mapped by the proxy
                // listener (never crosses the ACP wire).
                let text = self
                    .agents
                    .mcp
                    .servers
                    .values()
                    .filter_map(|server| server.error.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                let _ = reply.send(Ok(ClientRequestResponse::TerminalOutput(
                    TerminalOutputResponse::new(text, false),
                )));
            }
        }
    }
}
