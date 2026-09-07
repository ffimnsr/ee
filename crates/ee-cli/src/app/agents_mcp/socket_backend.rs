//! Socket proxy backend implementing `EeProxyBackend`.
use super::proxy_calls::ProxyCall;
use super::*;

pub(super) struct SocketProxyBackend {
    pub(super) inner: std::sync::Mutex<SocketProxyState>,
}

/// The socket state (writer + reader share the same connection).
pub(super) struct SocketProxyState {
    pub(super) writer: std::io::BufWriter<std::os::unix::net::UnixStream>,
    pub(super) reader: std::io::BufReader<std::os::unix::net::UnixStream>,
}

impl SocketProxyBackend {
    pub(super) fn connect(socket: &PathBuf, token: &str) -> std::io::Result<Self> {
        use std::io::Write;
        let stream = std::os::unix::net::UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(120)))?;
        let mut writer = std::io::BufWriter::new(stream.try_clone()?);
        writeln!(writer, "{token}")?;
        writer.flush()?;
        Ok(Self {
            inner: std::sync::Mutex::new(SocketProxyState {
                writer,
                reader: std::io::BufReader::new(stream),
            }),
        })
    }

    pub(super) fn call_value(&self, call: ProxyCall) -> Result<serde_json::Value, String> {
        use std::io::{BufRead, Write};
        let mut state = self.inner.lock().expect("proxy socket poisoned");
        let frame = serde_json::json!({ "id": 1, "params": call });
        writeln!(state.writer, "{frame}").map_err(|error| error.to_string())?;
        state.writer.flush().map_err(|error| error.to_string())?;
        let mut response = String::new();
        let read = state.reader.read_line(&mut response).map_err(|error| error.to_string())?;
        if read == 0 {
            return Err(String::from("editor closed the proxy connection"));
        }
        if response.len() > PROXY_MAX_FRAME_BYTES {
            return Err(String::from("proxy reply exceeds the frame cap"));
        }
        let value: serde_json::Value =
            serde_json::from_str(response.trim_end()).map_err(|error| error.to_string())?;
        let result =
            value.get("result").ok_or_else(|| String::from("proxy reply missing result"))?;
        if let Some(error) = result.get("error") {
            return Err(error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("proxy error")
                .to_string());
        }
        result.get("value").cloned().ok_or_else(|| String::from("proxy reply missing value"))
    }

    pub(super) fn call_text(&self, call: ProxyCall) -> Result<String, String> {
        self.call_value(call)?
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| String::from("proxy reply missing string value"))
    }

    pub(super) fn filesystem_call(
        &self,
        call: ProxyCall,
        tool: &str,
    ) -> Result<ee_mcp::FilesystemResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(call).map_err(|message| ee_mcp::ProxyToolError {
                message,
                is_permission_denied: false,
            })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy {tool} reply invalid: {error}"),
            is_permission_denied: false,
        })
    }
}

pub(super) fn proxy_value<T: serde::de::DeserializeOwned>(
    value: &Result<serde_json::Value, String>,
    operation: &str,
) -> Result<T, ee_mcp::ProxyToolError> {
    let value = value.as_ref().map_err(|message| ee_mcp::ProxyToolError {
        message: message.clone(),
        is_permission_denied: false,
    })?;
    serde_json::from_value(value.clone()).map_err(|error| ee_mcp::ProxyToolError {
        message: format!("proxy {operation} reply invalid: {error}"),
        is_permission_denied: false,
    })
}

impl ee_mcp::EeProxyBackend for SocketProxyBackend {
    fn remember_workspace_fact(
        &self,
        key: String,
        value: String,
    ) -> Result<ee_mcp::WorkspaceFactMutationResult, ee_mcp::ProxyToolError> {
        proxy_value(
            &self.call_value(ProxyCall::RememberWorkspaceFact { key, value }),
            "remember_workspace_fact",
        )
    }

    fn recall_workspace_facts(
        &self,
        query: String,
    ) -> Result<ee_mcp::WorkspaceFactsResult, ee_mcp::ProxyToolError> {
        proxy_value(
            &self.call_value(ProxyCall::RecallWorkspaceFacts { query }),
            "recall_workspace_facts",
        )
    }

    fn read_workspace_fact(
        &self,
        key: String,
    ) -> Result<ee_mcp::WorkspaceFact, ee_mcp::ProxyToolError> {
        proxy_value(&self.call_value(ProxyCall::ReadWorkspaceFact { key }), "read_workspace_fact")
    }

    fn forget_workspace_fact(
        &self,
        key: String,
    ) -> Result<ee_mcp::WorkspaceFactMutationResult, ee_mcp::ProxyToolError> {
        proxy_value(
            &self.call_value(ProxyCall::ForgetWorkspaceFact { key }),
            "forget_workspace_fact",
        )
    }

    fn list_workspace_facts(
        &self,
        limit: u32,
    ) -> Result<ee_mcp::WorkspaceFactsResult, ee_mcp::ProxyToolError> {
        proxy_value(
            &self.call_value(ProxyCall::ListWorkspaceFacts { limit }),
            "list_workspace_facts",
        )
    }

    fn retract_workspace_fact(
        &self,
        key: String,
    ) -> Result<ee_mcp::WorkspaceFactMutationResult, ee_mcp::ProxyToolError> {
        proxy_value(
            &self.call_value(ProxyCall::RetractWorkspaceFact { key }),
            "retract_workspace_fact",
        )
    }

    fn export_workspace_memory(
        &self,
        include_values: bool,
    ) -> Result<serde_json::Value, ee_mcp::ProxyToolError> {
        proxy_value(
            &self.call_value(ProxyCall::ExportWorkspaceMemory { include_values }),
            "export_workspace_memory",
        )
    }

    fn import_workspace_memory(
        &self,
        export_json: String,
    ) -> Result<serde_json::Value, ee_mcp::ProxyToolError> {
        proxy_value(
            &self.call_value(ProxyCall::ImportWorkspaceMemory { export_json }),
            "import_workspace_memory",
        )
    }

    fn clear_workspace_memory(&self) -> Result<serde_json::Value, ee_mcp::ProxyToolError> {
        proxy_value(&self.call_value(ProxyCall::ClearWorkspaceMemory), "clear_workspace_memory")
    }

    fn web_search(
        &self,
        request: ee_mcp::WebSearchRequest,
    ) -> Result<ee_mcp::WebSearchResult, ee_mcp::ProxyToolError> {
        proxy_value(&self.call_value(ProxyCall::WebSearch { query: request.query }), "web_search")
    }

    fn fetch_url(
        &self,
        request: ee_mcp::FetchUrlRequest,
    ) -> Result<ee_mcp::FetchUrlResult, ee_mcp::ProxyToolError> {
        proxy_value(&self.call_value(ProxyCall::FetchUrl { url: request.url }), "fetch_url")
    }

    fn browser_run(
        &self,
        request: ee_mcp::BrowserRunRequest,
    ) -> Result<ee_mcp::BrowserRunResult, ee_mcp::ProxyToolError> {
        proxy_value(&self.call_value(ProxyCall::BrowserRun { request }), "browser_run")
    }

    fn workspace_roots(&self) -> Result<ee_mcp::WorkspaceRootsResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::WorkspaceRoots).map_err(|message| {
                ee_mcp::ProxyToolError { message, is_permission_denied: false }
            })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy workspace_roots reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn list_directory(
        &self,
        path: String,
    ) -> Result<ee_mcp::ListDirectoryResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::ListDirectory { path }).map_err(|message| {
                ee_mcp::ProxyToolError { message, is_permission_denied: false }
            })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy list_directory reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn list_directory_all(
        &self,
        path: String,
    ) -> Result<ee_mcp::ListDirectoryAllResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::ListDirectoryAll { path }).map_err(|message| {
                ee_mcp::ProxyToolError { message, is_permission_denied: false }
            })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy list_directory_all reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn search_files(
        &self,
        pattern: String,
    ) -> Result<ee_mcp::SearchFilesResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::SearchFiles { pattern }).map_err(|message| {
                ee_mcp::ProxyToolError { message, is_permission_denied: false }
            })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy search_files reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn search_files_all(
        &self,
        pattern: String,
    ) -> Result<ee_mcp::SearchFilesAllResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::SearchFilesAll { pattern }).map_err(|message| {
                ee_mcp::ProxyToolError { message, is_permission_denied: false }
            })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy search_files_all reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn search_text(
        &self,
        query: String,
    ) -> Result<ee_mcp::SearchTextResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::SearchText { query }).map_err(|message| {
                ee_mcp::ProxyToolError { message, is_permission_denied: false }
            })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy search_text reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn search_text_regex(
        &self,
        pattern: String,
    ) -> Result<ee_mcp::SearchTextResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::SearchTextRegex { pattern }).map_err(|message| {
                ee_mcp::ProxyToolError { message, is_permission_denied: false }
            })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy search_text_regex reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn search_text_in_files(
        &self,
        query: String,
        file_glob: String,
    ) -> Result<ee_mcp::SearchTextResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::SearchTextInFiles { query, file_glob }).map_err(
                |message| ee_mcp::ProxyToolError { message, is_permission_denied: false },
            )?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy search_text_in_files reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn replace_text(
        &self,
        path: String,
        old_text: String,
        new_text: String,
    ) -> Result<ee_mcp::EditTextResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::ReplaceText { path, old_text, new_text }).map_err(
                |message| ee_mcp::ProxyToolError { message, is_permission_denied: false },
            )?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy replace_text reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn apply_patch(
        &self,
        path: String,
        edits: Vec<ee_mcp::TextEdit>,
    ) -> Result<ee_mcp::EditTextResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::ApplyPatch { path, edits }).map_err(|message| {
                ee_mcp::ProxyToolError { message, is_permission_denied: false }
            })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy apply_patch reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn create_text_file(
        &self,
        path: String,
        content: String,
    ) -> Result<ee_mcp::EditTextResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::CreateTextFile { path, content }).map_err(|message| {
                ee_mcp::ProxyToolError { message, is_permission_denied: false }
            })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy create_text_file reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn overwrite_text_file(
        &self,
        path: String,
        content: String,
    ) -> Result<ee_mcp::EditTextResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::OverwriteTextFile { path, content }).map_err(|message| {
                ee_mcp::ProxyToolError { message, is_permission_denied: false }
            })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy overwrite_text_file reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn create_directory(
        &self,
        path: String,
    ) -> Result<ee_mcp::FilesystemResult, ee_mcp::ProxyToolError> {
        self.filesystem_call(ProxyCall::CreateDirectory { path }, "create_directory")
    }

    fn delete_path(
        &self,
        path: String,
    ) -> Result<ee_mcp::FilesystemResult, ee_mcp::ProxyToolError> {
        self.filesystem_call(ProxyCall::DeletePath { path }, "delete_path")
    }

    fn copy_path(
        &self,
        source_path: String,
        destination_path: String,
    ) -> Result<ee_mcp::FilesystemResult, ee_mcp::ProxyToolError> {
        self.filesystem_call(ProxyCall::CopyPath { source_path, destination_path }, "copy_path")
    }

    fn move_path(
        &self,
        source_path: String,
        destination_path: String,
    ) -> Result<ee_mcp::FilesystemResult, ee_mcp::ProxyToolError> {
        self.filesystem_call(ProxyCall::MovePath { source_path, destination_path }, "move_path")
    }

    fn read_buffer(&self, path: String) -> Result<String, ee_mcp::ProxyToolError> {
        self.call_text(ProxyCall::ReadBuffer { path })
            .map_err(|message| ee_mcp::ProxyToolError { message, is_permission_denied: false })
    }

    fn read_buffer_lines(
        &self,
        path: String,
        line: u32,
        limit: u32,
    ) -> Result<String, ee_mcp::ProxyToolError> {
        self.call_text(ProxyCall::ReadBufferLines { path, line, limit })
            .map_err(|message| ee_mcp::ProxyToolError { message, is_permission_denied: false })
    }

    fn open_buffers(&self) -> Result<ee_mcp::OpenBuffersResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::OpenBuffers).map_err(|message| ee_mcp::ProxyToolError {
                message,
                is_permission_denied: false,
            })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy open_buffers reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn get_diagnostics(&self) -> Result<ee_mcp::DiagnosticsResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::GetDiagnostics).map_err(|message| {
                ee_mcp::ProxyToolError { message, is_permission_denied: false }
            })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy get_diagnostics reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn get_file_diagnostics(
        &self,
        path: String,
    ) -> Result<ee_mcp::DiagnosticsResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::GetFileDiagnostics { path }).map_err(|message| {
                ee_mcp::ProxyToolError { message, is_permission_denied: false }
            })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy get_file_diagnostics reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn document_symbols(
        &self,
        path: String,
    ) -> Result<ee_mcp::DocumentSymbolsResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::DocumentSymbols { path }).map_err(|message| {
                ee_mcp::ProxyToolError { message, is_permission_denied: false }
            })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy document_symbols reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn references(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> Result<ee_mcp::ReferencesResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::References { path, line, character }).map_err(
                |message| ee_mcp::ProxyToolError { message, is_permission_denied: false },
            )?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy references reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn list_code_actions(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> Result<ee_mcp::CodeActionsResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::ListCodeActions { path, line, character }).map_err(
                |message| ee_mcp::ProxyToolError { message, is_permission_denied: false },
            )?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy list_code_actions reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn apply_code_action(
        &self,
        path: String,
        action_id: String,
    ) -> Result<ee_mcp::EditTextResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::ApplyCodeAction { path, action_id }).map_err(|message| {
                ee_mcp::ProxyToolError { message, is_permission_denied: false }
            })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy apply_code_action reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn format_file(&self, path: String) -> Result<ee_mcp::EditTextResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::FormatFile { path }).map_err(|message| {
                ee_mcp::ProxyToolError { message, is_permission_denied: false }
            })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy format_file reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn preview_rename_symbol(
        &self,
        path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<ee_mcp::RenamePreviewResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::PreviewRenameSymbol { path, line, character, new_name })
                .map_err(|message| ee_mcp::ProxyToolError {
                    message,
                    is_permission_denied: false,
                })?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy preview_rename_symbol reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn rename_symbol(
        &self,
        path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<ee_mcp::WorkspaceEditResult, ee_mcp::ProxyToolError> {
        serde_json::from_value(
            self.call_value(ProxyCall::RenameSymbol { path, line, character, new_name }).map_err(
                |message| ee_mcp::ProxyToolError { message, is_permission_denied: false },
            )?,
        )
        .map_err(|error| ee_mcp::ProxyToolError {
            message: format!("proxy rename_symbol reply invalid: {error}"),
            is_permission_denied: false,
        })
    }

    fn git_status(&self) -> Result<ee_mcp::GitStatusResult, ee_mcp::ProxyToolError> {
        proxy_value(&self.call_value(ProxyCall::GitStatus), "git_status")
    }

    fn git_diff(&self) -> Result<ee_mcp::GitDiffResult, ee_mcp::ProxyToolError> {
        proxy_value(&self.call_value(ProxyCall::GitDiff), "git_diff")
    }

    fn git_diff_staged(&self) -> Result<ee_mcp::GitDiffResult, ee_mcp::ProxyToolError> {
        proxy_value(&self.call_value(ProxyCall::GitDiffStaged), "git_diff_staged")
    }

    fn git_diff_file(&self, path: String) -> Result<ee_mcp::GitDiffResult, ee_mcp::ProxyToolError> {
        proxy_value(&self.call_value(ProxyCall::GitDiffFile { path }), "git_diff_file")
    }

    fn changed_files(&self) -> Result<ee_mcp::ChangedFilesResult, ee_mcp::ProxyToolError> {
        proxy_value(&self.call_value(ProxyCall::ChangedFiles), "changed_files")
    }

    fn review_context(&self) -> Result<ee_mcp::ReviewContextResult, ee_mcp::ProxyToolError> {
        proxy_value(&self.call_value(ProxyCall::ReviewContext), "review_context")
    }

    fn project_instructions(
        &self,
    ) -> Result<ee_mcp::ProjectInstructionsResult, ee_mcp::ProxyToolError> {
        proxy_value(&self.call_value(ProxyCall::ProjectInstructions), "project_instructions")
    }

    fn save_note(
        &self,
        key: String,
        content: String,
    ) -> Result<ee_mcp::SessionNoteResult, ee_mcp::ProxyToolError> {
        proxy_value(&self.call_value(ProxyCall::SaveNote { key, content }), "save_note")
    }

    fn read_notes(&self) -> Result<ee_mcp::SessionNotesResult, ee_mcp::ProxyToolError> {
        proxy_value(&self.call_value(ProxyCall::ReadNotes), "read_notes")
    }

    fn read_note(&self, key: String) -> Result<ee_mcp::SessionNoteResult, ee_mcp::ProxyToolError> {
        proxy_value(&self.call_value(ProxyCall::ReadNote { key }), "read_note")
    }

    fn file_dependency_map(
        &self,
        path: String,
    ) -> Result<ee_mcp::FileDependencyMapResult, ee_mcp::ProxyToolError> {
        proxy_value(&self.call_value(ProxyCall::FileDependencyMap { path }), "file_dependency_map")
    }

    fn symbol_dependency_map(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> Result<ee_mcp::SymbolDependencyMapResult, ee_mcp::ProxyToolError> {
        proxy_value(
            &self.call_value(ProxyCall::SymbolDependencyMap { path, line, character }),
            "symbol_dependency_map",
        )
    }

    fn read_text_file(
        &self,
        path: String,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<String, ee_mcp::ProxyToolError> {
        let call = ProxyCall::ReadTextFile { path, line, limit };
        self.call_text(call)
            .map_err(|message| ee_mcp::ProxyToolError { message, is_permission_denied: false })
    }

    fn write_text_file(&self, path: String, content: String) -> Result<(), ee_mcp::ProxyToolError> {
        let call = ProxyCall::WriteTextFile { path, content };
        let _ = self
            .call_text(call)
            .map_err(|message| ee_mcp::ProxyToolError { message, is_permission_denied: false })?;
        Ok(())
    }

    fn terminal_create(
        &self,
        command: String,
        args: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
    ) -> Result<String, ee_mcp::ProxyToolError> {
        let call = ProxyCall::TerminalCreate { command, args, cwd, env };
        self.call_text(call)
            .map_err(|message| ee_mcp::ProxyToolError { message, is_permission_denied: false })
    }

    fn terminal_output(
        &self,
        _terminal_id: String,
    ) -> Result<ee_mcp::TerminalOutputResult, ee_mcp::ProxyToolError> {
        Err(ee_mcp::ProxyToolError {
            message: String::from("terminal lifecycle tools require ACP-native MCP proxy mode"),
            is_permission_denied: false,
        })
    }

    fn terminal_wait(
        &self,
        _terminal_id: String,
    ) -> Result<ee_mcp::TerminalWaitResult, ee_mcp::ProxyToolError> {
        Err(ee_mcp::ProxyToolError {
            message: String::from("terminal lifecycle tools require ACP-native MCP proxy mode"),
            is_permission_denied: false,
        })
    }

    fn terminal_kill(&self, _terminal_id: String) -> Result<(), ee_mcp::ProxyToolError> {
        Err(ee_mcp::ProxyToolError {
            message: String::from("terminal lifecycle tools require ACP-native MCP proxy mode"),
            is_permission_denied: false,
        })
    }

    fn terminal_release(&self, _terminal_id: String) -> Result<(), ee_mcp::ProxyToolError> {
        Err(ee_mcp::ProxyToolError {
            message: String::from("terminal lifecycle tools require ACP-native MCP proxy mode"),
            is_permission_denied: false,
        })
    }

    fn supported_tools(&self) -> Option<Vec<String>> {
        Some(
            ee_mcp::tool_names_for_transport(ee_mcp::ToolTransport::Stdio)
                .into_iter()
                .map(ToOwned::to_owned)
                .collect(),
        )
    }

    fn diagnostics(&self) -> Vec<String> {
        self.call_text(ProxyCall::Diagnostics)
            .map(|text| text.lines().map(ToOwned::to_owned).collect())
            .unwrap_or_default()
    }
}
