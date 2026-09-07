//! EeProxyBackend for the host proxy backend: the full trait surface.
use super::*;

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
